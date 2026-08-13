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

// ── Import jobs ────────────────────────────────────────────────────────────────

/// `POST /imports` — upload a CSV and get back a draft job with inferred columns.
///
/// The file arrives as a JSON string, not multipart (see
/// [`crate::models::CreateImportRequest::csv`]). The size ceiling is the store's, so
/// an oversized upload comes back as a 422 naming the limit rather than a truncated
/// parse.
async fn create_import(
    State(state): State<AppState>,
    Json(body): Json<CreateImportRequest>,
) -> ApiResult<Json<ImportJob>> {
    if body.csv.trim().is_empty() {
        return Err(ApiError::bad_request("that upload contained no CSV text"));
    }
    // A delimiter is ONE character. `store::create_import` takes `.chars().next()` and
    // silently sniffs when the string is empty, so `""` and `"||"` would both "work"
    // while doing something the caller did not ask for.
    if let Some(delimiter) = body.delimiter.as_deref() {
        if delimiter.chars().count() != 1 {
            return Err(ApiError::bad_request(
                "a delimiter must be exactly one character",
            ));
        }
    }
    // Resolved only so an unknown object is a 404 rather than the store's `bail!`,
    // which would surface as an opaque 500. The request is then passed through
    // unchanged — `create_import` resolves the same id-or-slug itself, and cloning the
    // body to canonicalise one field would copy up to 16 MiB of CSV to save a lookup.
    if state.store.get_object(&body.object_id).await?.is_none() {
        return Err(ApiError::not_found("object"));
    }
    match state
        .store
        .create_import(&body, state.config.max_import_bytes)
        .await?
    {
        Ok(job) => Ok(Json(job)),
        Err(errors) => Err(ApiError::validation(errors)),
    }
}

#[derive(Debug, Default, Deserialize)]
struct ImportListQuery {
    #[serde(default)]
    object_id: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /imports` — recent jobs, newest first.
async fn list_imports(
    State(state): State<AppState>,
    Query(query): Query<ImportListQuery>,
) -> ApiResult<Json<Value>> {
    let limit = state.config.clamp_limit(query.limit);
    // `list_imports` filters on the `object_id` COLUMN, so a slug would match nothing
    // and read as "this object has never been imported to". Resolve it to the id, or
    // say the object does not exist.
    let object_id = match query
        .object_id
        .as_deref()
        .map(str::trim)
        .filter(|o| !o.is_empty())
    {
        Some(raw) => Some(
            state
                .store
                .get_object(raw)
                .await?
                .ok_or_else(|| ApiError::not_found("object"))?
                .id,
        ),
        None => None,
    };
    let imports = state.store.list_imports(object_id.as_deref(), limit).await?;
    Ok(Json(json!({ "imports": imports })))
}

/// `GET /imports/:import_id`. The raw CSV is never serialized — see [`ImportJob`].
async fn get_import(
    State(state): State<AppState>,
    Path(import_id): Path<String>,
) -> ApiResult<Json<ImportJob>> {
    state
        .store
        .get_import(&import_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("import"))
}

/// `DELETE /imports/:import_id`. Deletes the JOB, never the records it wrote — an
/// applied import is history, and undoing it is a merge/delete problem with its own
/// routes.
async fn delete_import(
    State(state): State<AppState>,
    Path(import_id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !state.store.delete_import(&import_id).await? {
        return Err(ApiError::not_found("import"));
    }
    Ok(Json(json!({ "ok": true })))
}

// ── Mapping ────────────────────────────────────────────────────────────────────

/// `GET /imports/:import_id/mapping` — everything the mapping screen needs in one
/// call: the parsed columns (with samples, the store's field suggestion and this
/// module's type guess), the mapping saved so far, the dedupe rule, and the object's
/// full field list to pick from.
///
/// One call rather than "fetch the job, then fetch the schema": the two are read
/// together every single time, and splitting them is a guaranteed extra round-trip on
/// a screen the user is already waiting on.
async fn get_mapping(
    State(state): State<AppState>,
    Path(import_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let job = load_job(&state, &import_id).await?;
    let fields = state.store.list_fields(&job.object_id).await?;
    let columns: Vec<Value> = job
        .columns
        .iter()
        .map(|column| {
            json!({
                "index": column.index,
                "name": column.name,
                "samples": column.samples,
                "suggested_field_id": column.suggested_field_id,
                // The store guesses WHICH field; this guesses what TYPE a new field
                // for this column should be. Advisory in both cases — nothing is
                // written until the mapping is saved.
                "suggested_field_type": infer_field_type(&column.samples).as_str(),
            })
        })
        .collect();
    Ok(Json(json!({
        "import_id": job.id,
        "object_id": job.object_id,
        "status": job.status,
        "row_count": job.row_count,
        "has_header": job.has_header,
        "delimiter": job.delimiter,
        "columns": columns,
        "mappings": job.mappings,
        "dedupe": job.dedupe,
        "fields": fields,
    })))
}

/// One column's destination, as the mapping screen sends it.
///
/// A superset of [`ImportMapping`]: the wire form the store persists is
/// `{column_index, field_id}`, and this adds the third option the store has no
/// concept of — *make me a field for this column*.
#[derive(Debug, Default, Deserialize)]
struct MappingEntryBody {
    column_index: usize,
    /// An existing field's id or slug. `None` + no `create_field` ⇒ ignore the column.
    #[serde(default)]
    field_id: Option<String>,
    /// Create a field and bind this column to it.
    #[serde(default)]
    create_field: Option<NewFieldBody>,
}

/// The "create a field from this column" form.
///
/// Not [`CreateFieldRequest`] directly, because every member here is optional and that
/// type's `field_type` has a `Default` — so an omitted type and an explicit `"text"`
/// would be indistinguishable, and the inference this module exists to provide could
/// never fire.
#[derive(Debug, Default, Deserialize)]
struct NewFieldBody {
    /// Derived from `name`, then from the column header, when absent.
    #[serde(default)]
    slug: Option<String>,
    /// Defaults to the column header.
    #[serde(default)]
    name: Option<String>,
    /// Inferred from the column's sample values when absent.
    #[serde(default)]
    field_type: Option<FieldType>,
    #[serde(default)]
    config: Option<FieldConfig>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    is_required: bool,
    #[serde(default)]
    is_unique: bool,
}

#[derive(Debug, Default, Deserialize)]
struct MappingBody {
    #[serde(default)]
    mappings: Vec<MappingEntryBody>,
    #[serde(default)]
    dedupe: ImportDedupe,
}

/// `PUT /imports/:import_id/mapping` — save the column → field decisions and the
/// dedupe rule, creating any requested fields first.
///
/// The order of work is deliberate and is the reason this is not three lines:
///
/// 1. validate everything that can be validated without writing,
/// 2. create the new fields,
/// 3. save the mapping.
///
/// Field creation is not transactional with the mapping save, so a rejection
/// discovered in step 3 would leave orphan fields on the object. Doing all the
/// cheap rejections first means the only way to orphan a field is a genuine
/// mid-request failure.
async fn put_mapping(
    State(state): State<AppState>,
    Path(import_id): Path<String>,
    Json(body): Json<MappingBody>,
) -> ApiResult<Json<Value>> {
    let job = load_job(&state, &import_id).await?;
    // `set_import_mapping` refuses an applied job by matching zero rows, which is
    // indistinguishable from "no such job". Answer the honest status here.
    reject_if_applied(&job, "remapped")?;

    let column_count = job.columns.len();
    let mut seen_columns: HashSet<usize> = HashSet::new();
    let mut seen_fields: HashSet<String> = HashSet::new();
    // (column index, resolved field id) for entries that already have a home, and
    // (column index, request) for the ones that need a field built.
    let mut resolved: Vec<ImportMapping> = Vec::with_capacity(body.mappings.len());
    let mut to_create: Vec<(usize, CreateFieldRequest)> = Vec::new();

    for entry in &body.mappings {
        if entry.column_index >= column_count {
            return Err(ApiError::bad_request(format!(
                "column {} is past the end of this file, which has {column_count} columns",
                entry.column_index
            )));
        }
        if !seen_columns.insert(entry.column_index) {
            return Err(ApiError::bad_request(format!(
                "column {} is mapped twice",
                entry.column_index
            )));
        }
        let field_ref = entry
            .field_id
            .as_deref()
            .map(str::trim)
            .filter(|f| !f.is_empty());
        match (field_ref, &entry.create_field) {
            (Some(_), Some(_)) => {
                return Err(ApiError::bad_request(format!(
                    "column {} maps to an existing field AND asks for a new one — pick one",
                    entry.column_index
                )));
            }
            (Some(reference), None) => {
                // Resolve rather than trust: `plan_import` looks the mapping up with
                // `filter_map`, so an id that does not exist on this object is not an
                // error there — it is a column that silently imports nothing.
                let field = state
                    .store
                    .resolve_field(&job.object_id, reference)
                    .await?
                    .ok_or_else(|| {
                        ApiError::bad_request(format!(
                            "\"{reference}\" is not a field on this object"
                        ))
                    })?;
                if !seen_fields.insert(field.id.clone()) {
                    return Err(ApiError::bad_request(format!(
                        "two columns map to \"{}\" — the second would overwrite the first",
                        field.name
                    )));
                }
                resolved.push(ImportMapping {
                    column_index: entry.column_index,
                    field_id: Some(field.id),
                });
            }
            (None, Some(new_field)) => {
                let column = &job.columns[entry.column_index];
                to_create.push((entry.column_index, new_field_request(new_field, column)?));
            }
            (None, None) => {
                // An explicit "ignore this column". Persisted rather than dropped so
                // the mapping screen can tell "decided to skip" from "not looked at".
                resolved.push(ImportMapping {
                    column_index: entry.column_index,
                    field_id: None,
                });
            }
        }
    }

    // Step 2: build the new fields. A duplicate slug comes back from the store as a
    // `not_unique` validation error, which is exactly the 422 the form wants.
    let mut created_fields: Vec<Field> = Vec::with_capacity(to_create.len());
    for (column_index, request) in to_create {
        match state
            .store
            .create_field(&job.object_id, None, &request)
            .await?
        {
            Ok(field) => {
                resolved.push(ImportMapping {
                    column_index,
                    field_id: Some(field.id.clone()),
                });
                created_fields.push(field);
            }
            Err(errors) => return Err(ApiError::validation(errors)),
        }
    }
    resolved.sort_by_key(|m| m.column_index);

    let dedupe = resolve_dedupe(&state, &job, &body.dedupe, &resolved).await?;
    let saved = state
        .store
        .set_import_mapping(
            &import_id,
            &SetImportMappingRequest {
                mappings: resolved,
                dedupe,
            },
        )
        .await?
        // Only reachable if the job was deleted or applied between `load_job` and
        // here — a real race, not a routine outcome.
        .ok_or_else(|| ApiError::conflict("this import changed while it was being mapped"))?;

    // The job at the top level plus one additive key: a panel that just created three
    // fields has a stale schema and needs to know without diffing.
    let mut payload = serde_json::to_value(&saved)?;
    payload["created_fields"] = serde_json::to_value(&created_fields)?;
    Ok(Json(payload))
}

/// Turn the create-a-field form into a real [`CreateFieldRequest`], filling every
/// omitted member from the column it was raised for.
fn new_field_request(body: &NewFieldBody, column: &ImportColumn) -> ApiResult<CreateFieldRequest> {
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(column.name.trim())
        .to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request(
            "a new field needs a name, and this column's header is blank",
        ));
    }
    // Slug from the explicit value, then the name, then the header. `slugify` is what
    // the store's own suggester uses, so a field created here is named the way a field
    // created by hand from the same header would be.
    let slug = body
        .slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .or_else(|| slugify(&name))
        .or_else(|| slugify(&column.name))
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "\"{name}\" does not contain anything usable as a field slug"
            ))
        })?;
    Ok(CreateFieldRequest {
        slug,
        name,
        field_type: body
            .field_type
            .unwrap_or_else(|| infer_field_type(&column.samples)),
        config: body.config.clone().unwrap_or_default(),
        description: body.description.clone(),
        is_required: body.is_required,
        // Rejected by the store for a LIST field; harmless here, and the honest way to
        // say "this column is the email and it must stay unique".
        is_unique: body.is_unique,
        position: None,
    })
}

/// Resolve the dedupe keys against the object, and refuse a key that is not mapped.
///
/// The refusal is the point. `find_import_match` bails out to "no match" the moment a
/// row is missing one of its match values, so a dedupe key bound to no column makes
/// EVERY row a create — the user asked for dedupe, watched it run, and got a duplicate
/// of every record. Nothing in the store's types can catch that; it is a relationship
/// between two fields of the same request.
async fn resolve_dedupe(
    state: &AppState,
    job: &ImportJob,
    requested: &ImportDedupe,
    mappings: &[ImportMapping],
) -> ApiResult<ImportDedupe> {
    let mapped: HashSet<&str> = mappings
        .iter()
        .filter_map(|m| m.field_id.as_deref())
        .collect();
    let mut match_field_ids = Vec::with_capacity(requested.match_field_ids.len());
    for reference in &requested.match_field_ids {
        let reference = reference.trim();
        if reference.is_empty() {
            continue;
        }
        let field = state
            .store
            .resolve_field(&job.object_id, reference)
            .await?
            .ok_or_else(|| {
                ApiError::bad_request(format!("\"{reference}\" is not a field on this object"))
            })?;
        if !mapped.contains(field.id.as_str()) {
            return Err(ApiError::bad_request(format!(
                "\"{}\" is a dedupe key but no column maps to it, so nothing would ever match",
                field.name
            )));
        }
        if !match_field_ids.contains(&field.id) {
            match_field_ids.push(field.id);
        }
    }
    Ok(ImportDedupe {
        match_field_ids,
        strategy: requested.strategy,
    })
}

// ── Preview & apply ────────────────────────────────────────────────────────────

/// `POST /imports/:import_id/preview` — the dry run. Writes nothing to `records`.
async fn preview_import(
    State(state): State<AppState>,
    Path(import_id): Path<String>,
) -> ApiResult<Json<ImportPreview>> {
    let job = load_job(&state, &import_id).await?;
    reject_if_applied(&job, "previewed")?;
    require_a_mapping(&job)?;
    state
        .store
        .dry_run_import(&import_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("import"))
}

/// `POST /imports/:import_id/apply` — write the rows, then announce them.
///
/// **Idempotent per job**, enforced by the job's own status: the store stamps
/// `applied` inside the same transaction that writes the records, so a second apply
/// cannot double-import even if two clients race. The second caller gets a 409 and can
/// read the original outcome from `GET /imports/:id` — an import applied twice by
/// accident is a duplicate-cleanup afternoon, so this is the one place a retry must
/// NOT be transparent.
///
/// The response reports progress the only way a single transaction can: as the final
/// tally. There is no partial state to stream — the whole file lands or none of it
/// does, which is what makes a failed import a retry rather than a reconciliation.
async fn apply_import(
    State(state): State<AppState>,
    Path(import_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let job = load_job(&state, &import_id).await?;
    reject_if_applied(&job, "applied")?;
    require_a_mapping(&job)?;

    let result = state
        .store
        .apply_import(&import_id)
        .await?
        .ok_or_else(|| ApiError::not_found("import"))?;

    // Events AFTER the commit, never before: a hook that reacts by reading the record
    // back must not lose the race, and an event for a row that failed to commit is
    // unrecallable.
    let emitted = emit_import_events(&state, &job.object_id, &result).await?;

    // Re-read so the caller sees the job carrying its `result` and `applied` status,
    // rather than the pre-apply copy this handler validated.
    let saved = state.store.get_import(&import_id).await?;
    Ok(Json(json!({
        "import": saved,
        "result": result,
        "events_emitted": emitted,
    })))
}

/// Raise one `record.created` / `record.updated` per row the apply touched.
///
/// Returns how many events actually went out, which is not always
/// `created + updated` — see [`EVENT_FANOUT_LIMIT`].
///
/// `record_updated` is called with an EMPTY change list here, deliberately: the
/// import's transaction does not retain a per-row diff, and inventing one would mean
/// re-reading each record's before-state after it had already been overwritten. A
/// consumer that needs the diff reads the record; a consumer that needs to know a
/// record moved gets exactly that.
async fn emit_import_events(
    state: &AppState,
    object_id: &str,
    result: &ImportResult,
) -> ApiResult<usize> {
    if result.created_record_ids.is_empty() && result.updated_record_ids.is_empty() {
        return Ok(0);
    }
    let Some(object) = state.store.get_object(object_id).await? else {
        // The object vanished between the apply and the emit. The records are written;
        // failing the response now would tell the caller the import did not happen.
        tracing::warn!(object_id, "ryu-crm: import applied but its object is gone; no events");
        return Ok(0);
    };

    let mut emitted = 0usize;
    for record_id in result
        .created_record_ids
        .iter()
        .take(EVENT_FANOUT_LIMIT)
    {
        if let Some(record) = state.store.get_record(record_id).await? {
            events::record_created(&state.events, &record, &object).await;
            emitted += 1;
        }
    }
    let remaining = EVENT_FANOUT_LIMIT.saturating_sub(emitted);
    for record_id in result.updated_record_ids.iter().take(remaining) {
        if let Some(record) = state.store.get_record(record_id).await? {
            events::record_updated(&state.events, &record, &object, &[]).await;
            emitted += 1;
        }
    }

    let touched = result.created_record_ids.len() + result.updated_record_ids.len();
    if touched > emitted {
        tracing::warn!(
            touched,
            emitted,
            limit = EVENT_FANOUT_LIMIT,
            "ryu-crm: import exceeded the event fan-out cap; the remaining rows were written but not announced"
        );
    }
    Ok(emitted)
}

// ── Export ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct ExportQuery {
    /// Full-text pre-filter, ANDed with the view's own filter exactly as the on-screen
    /// search box is — so "export what I am looking at" exports what they are looking
    /// at.
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    include_deleted: bool,
    /// Row ceiling, clamped to [`EXPORT_MAX_ROWS`].
    #[serde(default)]
    limit: Option<usize>,
    /// Output delimiter. One character; defaults to `,`.
    #[serde(default)]
    delimiter: Option<String>,
}

/// The response type: headers plus a body. A tuple rather than `Response::builder()`
/// so there is no infallible-in-practice `http::Error` to map into a 500.
type CsvResponse = ([(HeaderName, String); 5], String);

/// `GET /exports/views/:view_id` — the view's current result set as CSV.
async fn export_view(
    State(state): State<AppState>,
    Path(view_id): Path<String>,
    Query(query): Query<ExportQuery>,
) -> ApiResult<CsvResponse> {
    let delimiter = match query.delimiter.as_deref() {
        Some(raw) => {
            let mut chars = raw.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => c,
                _ => {
                    return Err(ApiError::bad_request(
                        "a delimiter must be exactly one character",
                    ))
                }
            }
        }
        None => ',',
    };
    let ceiling = query
        .limit
        .filter(|n| *n > 0)
        .unwrap_or(EXPORT_MAX_ROWS)
        .min(EXPORT_MAX_ROWS);
    let overrides = ViewQueryOverrides {
        search: query
            .search
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        include_deleted: query.include_deleted,
        ..Default::default()
    };

    // Paged rather than one big query: `run_view` uses the limit it is given verbatim,
    // and the store's page ceiling exists for a reason. Each call takes the store lock
    // once and releases it before the next — no future is held across another.
    let page_size = state.config.max_page_size.max(1);
    let mut records: Vec<Record> = Vec::new();
    let mut fields: Vec<Field> = Vec::new();
    let mut view_name = String::new();
    let mut total: i64 = 0;
    let mut truncated = false;
    let mut offset = 0usize;
    loop {
        let want = page_size.min(ceiling - records.len());
        let Some(result) = state
            .store
            .run_view(&view_id, &overrides, want, offset)
            .await?
        else {
            return Err(ApiError::not_found("view"));
        };
        if records.is_empty() {
            fields = result.fields;
            view_name = result.view.name;
            total = result.page.total;
        }
        let got = result.page.items.len();
        let has_more = result.page.has_more;
        records.extend(result.page.items);
        offset += got;
        if got == 0 || !has_more {
            break;
        }
        if records.len() >= ceiling {
            truncated = true;
            break;
        }
    }

    let body = render_csv(&fields, &records, delimiter);
    // `slugify` closes the alphabet to `[a-z0-9_]`, which is also what makes it safe
    // to interpolate into a header value — a view named `"; rm -rf /` cannot become
    // one.
    let filename = format!(
        "{}.csv",
        slugify(&view_name).unwrap_or_else(|| "export".to_string())
    );
    Ok((
        [
            (CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
            (
                HeaderName::from_static(HDR_EXPORT_ROWS),
                records.len().to_string(),
            ),
            (HeaderName::from_static(HDR_EXPORT_TOTAL), total.to_string()),
            (
                HeaderName::from_static(HDR_EXPORT_TRUNCATED),
                truncated.to_string(),
            ),
        ],
        body,
    ))
}

/// Header row plus one row per record.
///
/// `id` leads and the two timestamps trail, so the human-meaningful columns sit where
/// a spreadsheet opens. All three are unmapped on a re-import (they are reserved
/// slugs, which is why no field can ever be called `id`), so the file round-trips
/// without them fighting a real column.
fn render_csv(fields: &[Field], records: &[Record], delimiter: char) -> String {
    let mut out = String::new();
    let mut header: Vec<String> = Vec::with_capacity(fields.len() + 3);
    header.push("id".to_string());
    header.extend(fields.iter().map(|f| f.name.clone()));
    header.push("created_at".to_string());
    header.push("updated_at".to_string());
    write_row(&mut out, &header, delimiter);

    for record in records {
        let mut row: Vec<String> = Vec::with_capacity(fields.len() + 3);
        row.push(record.id.clone());
        for field in fields {
            row.push(export_cell(field, record.values.get(&field.slug)));
        }
        row.push(record.created_at.clone());
        row.push(record.updated_at.clone());
        write_row(&mut out, &row, delimiter);
    }
    out
}

fn write_row(out: &mut String, cells: &[String], delimiter: char) {
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push(delimiter);
        }
        out.push_str(&csv_cell(cell, delimiter));
    }
    out.push('\n');
}

/// Escape one cell for [`crate::store::parse_csv`] to read back identically.
///
/// Quoted when it holds the delimiter, a quote, a newline, or edge whitespace the
/// reader would trim on the way back in. Inside quotes, `"` doubles — the exact rule
/// that reader implements.
fn csv_cell(raw: &str, delimiter: char) -> String {
    let needs_quotes = raw.contains(delimiter)
        || raw.contains('"')
        || raw.contains('\n')
        || raw.contains('\r')
        || raw.trim() != raw;
    if !needs_quotes {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for ch in raw.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Render one stored value the way a human reads it AND the way this app's own
/// importer parses it back. Where those two pull apart, the round trip wins:
///
/// * **currency stays integer cents** — the validator's rule is "a JSON integer is
///   already cents", so `123456` re-imports as $1,234.56. Writing `1234.56` would
///   re-import correctly too (a float is major units), but a spreadsheet that reads
///   the column as a number and writes back `1234.6` would then silently lose a cent
///   per row, whereas cents are exact.
/// * **options carry LABELS** — `resolve_option` accepts an id or a label, and a CSV
///   full of `opt_deal_stage_proposal` is unreadable.
/// * **multi-valued cells are comma-joined** — `as_list` splits a string on commas, so
///   `Enterprise, Priority` comes back as the same two options.
/// * **relations carry record IDS** — titles are not unique and would re-import as a
///   bad relation target, which is a worse trade than an opaque column.
fn export_cell(field: &Field, value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    match value {
        Value::Null => String::new(),
        Value::String(s) => {
            if field.field_type.is_option_backed() {
                field
                    .config
                    .option(s)
                    .map(|o| o.label.clone())
                    .unwrap_or_else(|| s.clone())
            } else {
                s.clone()
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(s) if field.field_type.is_option_backed() => field
                    .config
                    .option(s)
                    .map(|o| o.label.clone())
                    .unwrap_or_else(|| s.clone()),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        // No field type stores an object today. Emitting compact JSON rather than
        // an empty cell means a schema that grows one is visible in the export
        // instead of silently blank.
        other => other.to_string(),
    }
}

// ── Column type inference ──────────────────────────────────────────────────────

/// Guess the field type a column wants, from its own sample values.
///
/// Advisory only — it decides what the "create a field" form opens on, never what an
/// existing field is (a field's type is immutable, by design). Every rule requires
/// EVERY sample to agree: one address in a column of names must not turn the column
/// into an email field, because the import would then reject every other row.
///
/// Option-backed types are never inferred. A `select` with no options rejects every
/// value, so guessing one would produce a field that cannot accept the column that
/// suggested it.
fn infer_field_type(samples: &[String]) -> FieldType {
    let samples: Vec<&str> = samples
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if samples.is_empty() {
        return FieldType::Text;
    }
    let all = |f: fn(&str) -> bool| samples.iter().all(|s| f(s));

    if all(is_boolean_word) {
        return FieldType::Checkbox;
    }
    if all(looks_like_email) {
        return FieldType::Email;
    }
    if all(looks_like_url) {
        return FieldType::Url;
    }
    if all(looks_like_money) {
        return FieldType::Currency;
    }
    if all(|s| s.parse::<f64>().is_ok()) {
        return FieldType::Number;
    }
    // `normalize_date` also accepts a full datetime (it truncates), so a bare-date
    // shape is what separates the two. Ask for the strict shape first.
    if all(|s| s.len() == 10 && normalize_date(s).as_deref() == Some(s)) {
        return FieldType::Date;
    }
    if all(|s| normalize_datetime(s).is_some()) {
        return FieldType::Datetime;
    }
    if samples.iter().any(|s| s.chars().count() > LONG_TEXT_SAMPLE_LEN) {
        return FieldType::LongText;
    }
    FieldType::Text
}

fn is_boolean_word(raw: &str) -> bool {
    matches!(
        raw.to_ascii_lowercase().as_str(),
        "true" | "false" | "yes" | "no" | "y" | "n"
    )
}

fn looks_like_email(raw: &str) -> bool {
    !raw.contains(char::is_whitespace)
        && raw.matches('@').count() == 1
        && raw
            .split_once('@')
            .is_some_and(|(user, host)| !user.is_empty() && host.contains('.'))
}

fn looks_like_url(raw: &str) -> bool {
    (raw.starts_with("http://") || raw.starts_with("https://"))
        && !raw.contains(char::is_whitespace)
}

/// A leading currency symbol is the only signal that separates "amount" from "number",
/// and it is the one every exporting CRM emits.
fn looks_like_money(raw: &str) -> bool {
    let Some(rest) = raw.strip_prefix(['$', '€', '£', '¥']) else {
        return false;
    };
    let digits: String = rest.chars().filter(|c| *c != ',' && *c != ' ').collect();
    !digits.is_empty() && digits.parse::<f64>().is_ok()
}

// ── Shared helpers ─────────────────────────────────────────────────────────────

async fn load_job(state: &AppState, import_id: &str) -> ApiResult<ImportJob> {
    state
        .store
        .get_import(import_id)
        .await?
        .ok_or_else(|| ApiError::not_found("import"))
}

/// An applied job is finished. Every mutating step checks this itself rather than
/// letting the store's `bail!` become a 500 (apply) or its zero-row `UPDATE` become a
/// 404 (mapping).
fn reject_if_applied(job: &ImportJob, verb: &str) -> ApiResult<()> {
    if job.status == ImportStatus::Applied {
        return Err(ApiError::conflict(format!(
            "this import has already been applied and cannot be {verb} — its outcome is on the job"
        )));
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
mod tests {
    use super::*;
    use crate::store::parse_csv;

    fn csv_body(object: &str, csv: &str) -> CreateImportRequest {
        CreateImportRequest {
            object_id: object.to_string(),
            filename: Some("people.csv".to_string()),
            csv: csv.to_string(),
            delimiter: None,
            has_header: None,
        }
    }

    async fn seed_person(state: &AppState, name: &str, email: &str) -> Record {
        let mut values = ValueBag::new();
        values.insert("name".to_string(), json!(name));
        values.insert("email".to_string(), json!(email));
        state
            .store
            .create_record(
                OBJ_PERSON,
                &CreateRecordRequest {
                    values,
                    created_by: None,
                },
            )
            .await
            .expect("store")
            .expect("valid")
    }

    /// Map every column by field slug, with no dedupe.
    fn straight_mapping(count: usize, slugs: &[&str]) -> MappingBody {
        MappingBody {
            mappings: (0..count)
                .map(|index| MappingEntryBody {
                    column_index: index,
                    field_id: slugs.get(index).map(|s| (*s).to_string()),
                    create_field: None,
                })
                .collect(),
            dedupe: ImportDedupe::default(),
        }
    }

    // ── Upload ──

    #[tokio::test]
    async fn upload_infers_columns_and_prefills_the_mapping() {
        let state = AppState::in_memory().expect("state");
        let job = create_import(
            State(state.clone()),
            Json(csv_body(
                "person",
                "Name,Email,Job Title\nAda Lovelace,ada@example.com,Analyst\nGrace Hopper,grace@example.com,Admiral\n",
            )),
        )
        .await
        .expect("upload")
        .0;

        assert_eq!(job.row_count, 2, "the header is not a data row");
        assert_eq!(job.columns.len(), 3);
        assert_eq!(job.status, ImportStatus::Draft);
        // The store's suggester matched the headers, and the mapping was pre-filled
        // from those suggestions.
        assert_eq!(
            job.columns[1].suggested_field_id.as_deref(),
            Some(FLD_PERSON_EMAIL)
        );
        assert_eq!(
            job.mappings[1].field_id.as_deref(),
            Some(FLD_PERSON_EMAIL),
            "a matching header should not need a human"
        );
    }

    #[tokio::test]
    async fn upload_404s_on_an_unknown_object_instead_of_500ing() {
        let state = AppState::in_memory().expect("state");
        let error = create_import(
            State(state.clone()),
            Json(csv_body("aliens", "Name\nAda\n")),
        )
        .await
        .expect_err("unknown object");
        assert!(matches!(error, ApiError::NotFound(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn upload_rejects_a_multi_character_delimiter() {
        let state = AppState::in_memory().expect("state");
        let mut body = csv_body("person", "Name\nAda\n");
        body.delimiter = Some("||".to_string());
        let error = create_import(State(state.clone()), Json(body))
            .await
            .expect_err("bad delimiter");
        assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
    }

    // ── Mapping ──

    #[tokio::test]
    async fn mapping_rejects_a_column_past_the_end_of_the_file() {
        let state = AppState::in_memory().expect("state");
        let job = create_import(
            State(state.clone()),
            Json(csv_body("person", "Name\nAda Lovelace\n")),
        )
        .await
        .expect("upload")
        .0;

        let error = put_mapping(
            State(state.clone()),
            Path(job.id.clone()),
            Json(MappingBody {
                mappings: vec![MappingEntryBody {
                    column_index: 7,
                    field_id: Some("name".to_string()),
                    create_field: None,
                }],
                dedupe: ImportDedupe::default(),
            }),
        )
        .await
        .expect_err("out of range");
        assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn mapping_rejects_an_unknown_field_rather_than_silently_ignoring_the_column() {
        let state = AppState::in_memory().expect("state");
        let job = create_import(
            State(state.clone()),
            Json(csv_body("person", "Name\nAda Lovelace\n")),
        )
        .await
        .expect("upload")
        .0;

        let error = put_mapping(
            State(state.clone()),
            Path(job.id.clone()),
            Json(straight_mapping(1, &["nonexistent_field"])),
        )
        .await
        .expect_err("unknown field");
        assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn mapping_rejects_two_columns_pointing_at_one_field() {
        let state = AppState::in_memory().expect("state");
        let job = create_import(
            State(state.clone()),
            Json(csv_body("person", "First,Second\nAda,Lovelace\n")),
        )
        .await
        .expect("upload")
        .0;

        let error = put_mapping(
            State(state.clone()),
            Path(job.id.clone()),
            Json(straight_mapping(2, &["name", "name"])),
        )
        .await
        .expect_err("duplicate target");
        assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
    }

    /// The silent-no-op trap: a dedupe key nothing maps to makes `find_import_match`
    /// return "no match" for every row, so an import the user set up to MERGE
    /// duplicates instead creates one duplicate per row.
    #[tokio::test]
    async fn mapping_rejects_a_dedupe_key_that_no_column_feeds() {
        let state = AppState::in_memory().expect("state");
        let job = create_import(
            State(state.clone()),
            Json(csv_body("person", "Name\nAda Lovelace\n")),
        )
        .await
        .expect("upload")
        .0;

        let mut body = straight_mapping(1, &["name"]);
        body.dedupe = ImportDedupe {
            match_field_ids: vec!["email".to_string()],
            strategy: DedupeStrategy::Update,
        };
        let error = put_mapping(State(state.clone()), Path(job.id.clone()), Json(body))
            .await
            .expect_err("unmapped dedupe key");
        assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn mapping_creates_a_field_for_a_column_with_no_home() {
        let state = AppState::in_memory().expect("state");
        let job = create_import(
            State(state.clone()),
            Json(csv_body(
                "person",
                "Name,Slack Handle\nAda Lovelace,@ada\nGrace Hopper,@grace\n",
            )),
        )
        .await
        .expect("upload")
        .0;

        let saved = put_mapping(
            State(state.clone()),
            Path(job.id.clone()),
            Json(MappingBody {
                mappings: vec![
                    MappingEntryBody {
                        column_index: 0,
                        field_id: Some("name".to_string()),
                        create_field: None,
                    },
                    MappingEntryBody {
                        column_index: 1,
                        field_id: None,
                        // Nothing but the intent: name, slug and type all come from
                        // the column.
                        create_field: Some(NewFieldBody::default()),
                    },
                ],
                dedupe: ImportDedupe::default(),
            }),
        )
        .await
        .expect("mapping")
        .0;

        let created = saved["created_fields"]
            .as_array()
            .expect("created_fields")
            .clone();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0]["slug"], json!("slack_handle"));
        assert_eq!(created[0]["name"], json!("Slack Handle"));
        assert_eq!(created[0]["field_type"], json!("text"));

        // The column is bound to the field that was just built, not left dangling.
        let field_id = created[0]["id"].as_str().expect("id").to_string();
        let bound = saved["mappings"]
            .as_array()
            .expect("mappings")
            .iter()
            .find(|m| m["column_index"] == json!(1))
            .expect("column 1");
        assert_eq!(bound["field_id"], json!(field_id));
    }

    #[tokio::test]
    async fn mapping_refuses_a_column_that_asks_for_both_an_existing_and_a_new_field() {
        let state = AppState::in_memory().expect("state");
        let job = create_import(
            State(state.clone()),
            Json(csv_body("person", "Name\nAda Lovelace\n")),
        )
        .await
        .expect("upload")
        .0;

        let error = put_mapping(
            State(state.clone()),
            Path(job.id.clone()),
            Json(MappingBody {
                mappings: vec![MappingEntryBody {
                    column_index: 0,
                    field_id: Some("name".to_string()),
                    create_field: Some(NewFieldBody::default()),
                }],
                dedupe: ImportDedupe::default(),
            }),
        )
        .await
        .expect_err("ambiguous");
        assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
    }

    // ── Preview, apply, idempotency ──

    #[tokio::test]
    async fn preview_and_apply_agree_and_dedupe_on_email() {
        let state = AppState::in_memory().expect("state");
        let existing = seed_person(&state, "Ada Lovelace", "ada@example.com").await;

        let job = create_import(
            State(state.clone()),
            Json(csv_body(
                "person",
                "Name,Email,Job Title\nAda Lovelace,ada@example.com,Analyst\nGrace Hopper,grace@example.com,Admiral\n",
            )),
        )
        .await
        .expect("upload")
        .0;

        let mut body = straight_mapping(3, &["name", "email", "job_title"]);
        body.dedupe = ImportDedupe {
            match_field_ids: vec!["email".to_string()],
            strategy: DedupeStrategy::Update,
        };
        put_mapping(State(state.clone()), Path(job.id.clone()), Json(body))
            .await
            .expect("mapping");

        let preview = preview_import(State(state.clone()), Path(job.id.clone()))
            .await
            .expect("preview")
            .0;
        assert_eq!(preview.total_rows, 2);
        assert_eq!(preview.create_count, 1, "Grace is new");
        assert_eq!(preview.update_count, 1, "Ada matches on email");
        assert_eq!(preview.error_count, 0);
        assert!(
            preview.unmapped_columns.is_empty(),
            "every column was mapped"
        );

        let applied = apply_import(State(state.clone()), Path(job.id.clone()))
            .await
            .expect("apply")
            .0;
        // The apply must land exactly what the preview promised — same planner, so a
        // divergence here means someone gave one of them its own copy.
        assert_eq!(applied["result"]["created"], json!(1));
        assert_eq!(applied["result"]["updated"], json!(1));
        assert_eq!(applied["result"]["failed"], json!(0));
        assert_eq!(applied["import"]["status"], json!("applied"));
        assert_eq!(
            applied["result"]["updated_record_ids"],
            json!([existing.id]),
            "the update reused the seeded record rather than creating a twin"
        );

        // …and the update actually wrote through.
        let ada = state
            .store
            .get_record(&existing.id)
            .await
            .expect("store")
            .expect("record");
        assert_eq!(ada.values.get("job_title"), Some(&json!("Analyst")));
    }

    #[tokio::test]
    async fn a_second_apply_is_refused_rather_than_double_importing() {
        let state = AppState::in_memory().expect("state");
        let job = create_import(
            State(state.clone()),
            Json(csv_body("person", "Name\nAda Lovelace\n")),
        )
        .await
        .expect("upload")
        .0;
        put_mapping(
            State(state.clone()),
            Path(job.id.clone()),
            Json(straight_mapping(1, &["name"])),
        )
        .await
        .expect("mapping");
        apply_import(State(state.clone()), Path(job.id.clone()))
            .await
            .expect("first apply");

        let error = apply_import(State(state.clone()), Path(job.id.clone()))
            .await
            .expect_err("second apply");
        assert!(matches!(error, ApiError::Conflict(_)), "got {error:?}");

        // Remapping an applied job is refused for the same reason, and with the same
        // status — `set_import_mapping` would otherwise report it as a 404.
        let error = put_mapping(
            State(state.clone()),
            Path(job.id.clone()),
            Json(straight_mapping(1, &["name"])),
        )
        .await
        .expect_err("remap after apply");
        assert!(matches!(error, ApiError::Conflict(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn an_unmapped_import_is_refused_before_it_fails_every_row() {
        let state = AppState::in_memory().expect("state");
        let job = create_import(
            State(state.clone()),
            // A header the suggester cannot place, so nothing is pre-mapped.
            Json(csv_body("person", "Zxq\nsomething\n")),
        )
        .await
        .expect("upload")
        .0;
        assert!(job.mappings.iter().all(|m| m.field_id.is_none()));

        let error = preview_import(State(state.clone()), Path(job.id.clone()))
            .await
            .expect_err("nothing mapped");
        assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
        let error = apply_import(State(state.clone()), Path(job.id))
            .await
            .expect_err("nothing mapped");
        assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
    }

    /// Currency and dates are the two places the import's coercion is invisible from
    /// the wire types: a `"$1,234.56"` cell must land as 123456 CENTS, and a
    /// `2026-03-31` close date must stay a bare calendar day.
    #[tokio::test]
    async fn import_coerces_money_to_cents_and_dates_to_calendar_days() {
        let state = AppState::in_memory().expect("state");
        let job = create_import(
            State(state.clone()),
            Json(csv_body(
                "deal",
                "Name,Stage,Amount,Close Date\nBig One,Proposal,\"$1,234.56\",2026-03-31\n",
            )),
        )
        .await
        .expect("upload")
        .0;
        put_mapping(
            State(state.clone()),
            Path(job.id.clone()),
            Json(straight_mapping(4, &["name", "stage", "amount", "close_date"])),
        )
        .await
        .expect("mapping");
        let applied = apply_import(State(state.clone()), Path(job.id))
            .await
            .expect("apply")
            .0;

        let record_id = applied["result"]["created_record_ids"][0]
            .as_str()
            .expect("created id")
            .to_string();
        let record = state
            .store
            .get_record(&record_id)
            .await
            .expect("store")
            .expect("record");
        assert_eq!(record.values.get("amount"), Some(&json!(123_456)));
        assert_eq!(record.values.get("close_date"), Some(&json!("2026-03-31")));
        // "Proposal" is a LABEL; it must have resolved to the option id.
        assert_eq!(
            record.values.get("stage"),
            Some(&json!(OPT_DEAL_STAGE_PROPOSAL))
        );
    }

    // ── Type inference ──

    #[test]
    fn column_types_are_inferred_only_when_every_sample_agrees() {
        assert_eq!(
            infer_field_type(&["ada@example.com".into(), "grace@example.com".into()]),
            FieldType::Email
        );
        assert_eq!(
            infer_field_type(&["https://acme.com".into()]),
            FieldType::Url
        );
        assert_eq!(infer_field_type(&["$1,234.56".into()]), FieldType::Currency);
        assert_eq!(infer_field_type(&["42".into(), "7.5".into()]), FieldType::Number);
        assert_eq!(infer_field_type(&["2026-03-31".into()]), FieldType::Date);
        assert_eq!(
            infer_field_type(&["2026-03-31T09:00:00Z".into()]),
            FieldType::Datetime
        );
        assert_eq!(infer_field_type(&["yes".into(), "no".into()]), FieldType::Checkbox);
        assert_eq!(infer_field_type(&[]), FieldType::Text);
        assert_eq!(
            infer_field_type(&["x".repeat(LONG_TEXT_SAMPLE_LEN + 1)]),
            FieldType::LongText
        );
        // One dissenting sample drops the whole column back to text — inferring
        // `email` here would reject every other row of the import.
        assert_eq!(
            infer_field_type(&["ada@example.com".into(), "Ada Lovelace".into()]),
            FieldType::Text
        );
        // Never guessed: an option-backed field with no options rejects everything.
        for samples in [vec!["Lead".to_string()], vec!["A".into(), "B".into()]] {
            let inferred = infer_field_type(&samples);
            assert!(
                !inferred.is_option_backed(),
                "{samples:?} inferred {inferred:?}"
            );
        }
    }

    // ── Export ──

    #[test]
    fn every_cell_survives_the_round_trip_through_the_parser() {
        let rows = vec![vec![
            "plain".to_string(),
            "has,comma".to_string(),
            "has \"quotes\"".to_string(),
            "line\nbreak".to_string(),
            "  padded  ".to_string(),
            String::new(),
        ]];
        let mut written = String::new();
        write_row(&mut written, &rows[0], ',');

        let parsed = parse_csv(&written, ',');
        assert_eq!(parsed.len(), 1, "one row in, one row out");
        assert_eq!(parsed[0], rows[0]);
    }

    #[test]
    fn a_delimiter_that_is_not_a_comma_is_escaped_against_itself() {
        let cells = vec!["a;b".to_string(), "plain".to_string()];
        let mut written = String::new();
        write_row(&mut written, &cells, ';');
        assert_eq!(written, "\"a;b\";plain\n");
        assert_eq!(parse_csv(&written, ';')[0], cells);
    }

    #[tokio::test]
    async fn export_renders_labels_for_options_and_leaves_currency_in_cents() {
        let state = AppState::in_memory().expect("state");
        let mut values = ValueBag::new();
        values.insert("name".to_string(), json!("Big One"));
        values.insert("stage".to_string(), json!(OPT_DEAL_STAGE_PROPOSAL));
        values.insert("amount".to_string(), json!(123_456));
        state
            .store
            .create_record(
                OBJ_DEAL,
                &CreateRecordRequest {
                    values,
                    created_by: None,
                },
            )
            .await
            .expect("store")
            .expect("valid");

        let (headers, body) = export_view(
            State(state.clone()),
            Path(VIEW_DEAL_ALL.to_string()),
            Query(ExportQuery::default()),
        )
        .await
        .expect("export");

        let rows = parse_csv(&body, ',');
        assert_eq!(rows.len(), 2, "header + one record");
        assert_eq!(rows[0][0], "id");
        // Headers carry field NAMES, which is what the importer's suggester matches
        // on — this is what makes an exported file re-import without hand-mapping.
        let stage_at = rows[0]
            .iter()
            .position(|h| h == "Stage")
            .expect("a Stage column");
        let amount_at = rows[0]
            .iter()
            .position(|h| h == "Amount")
            .expect("an Amount column");
        assert_eq!(rows[1][stage_at], "Proposal", "the label, not the option id");
        assert_eq!(rows[1][amount_at], "123456", "cents, not dollars");

        let rows_header = headers
            .iter()
            .find(|(name, _)| name.as_str() == HDR_EXPORT_ROWS)
            .expect("row-count header");
        assert_eq!(rows_header.1, "1");
        let truncated = headers
            .iter()
            .find(|(name, _)| name.as_str() == HDR_EXPORT_TRUNCATED)
            .expect("truncation header");
        assert_eq!(truncated.1, "false");
    }

    #[tokio::test]
    async fn exporting_an_unknown_view_is_a_404() {
        let state = AppState::in_memory().expect("state");
        let error = export_view(
            State(state.clone()),
            Path("view_nope".to_string()),
            Query(ExportQuery::default()),
        )
        .await
        .expect_err("unknown view");
        assert!(matches!(error, ApiError::NotFound(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn an_export_of_an_empty_view_is_still_a_header_row() {
        let state = AppState::in_memory().expect("state");
        let (_, body) = export_view(
            State(state.clone()),
            Path(VIEW_COMPANY_ALL.to_string()),
            Query(ExportQuery::default()),
        )
        .await
        .expect("export");
        let rows = parse_csv(&body, ',');
        assert_eq!(rows.len(), 1, "a header and nothing else");
        assert!(rows[0].contains(&"id".to_string()));
    }

    /// An export cut short by the row cap must SAY so — a truncated file that looks
    /// complete is the only real failure mode of a ceiling.
    #[tokio::test]
    async fn a_capped_export_reports_that_it_was_truncated() {
        let state = AppState::in_memory().expect("state");
        for n in 0..3 {
            let mut values = ValueBag::new();
            values.insert("name".to_string(), json!(format!("Company {n}")));
            state
                .store
                .create_record(
                    OBJ_COMPANY,
                    &CreateRecordRequest {
                        values,
                        created_by: None,
                    },
                )
                .await
                .expect("store")
                .expect("valid");
        }

        let (headers, body) = export_view(
            State(state.clone()),
            Path(VIEW_COMPANY_ALL.to_string()),
            Query(ExportQuery {
                limit: Some(2),
                ..Default::default()
            }),
        )
        .await
        .expect("export");
        assert_eq!(parse_csv(&body, ',').len(), 3, "header + two records");
        let truncated = headers
            .iter()
            .find(|(name, _)| name.as_str() == HDR_EXPORT_TRUNCATED)
            .expect("truncation header");
        assert_eq!(truncated.1, "true");
        let total = headers
            .iter()
            .find(|(name, _)| name.as_str() == HDR_EXPORT_TOTAL)
            .expect("total header");
        assert_eq!(total.1, "3", "the view's real size, not the page's");
    }
}
