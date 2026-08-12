//! Records: the row surface. Create, read, patch, trash, restore, query, relate,
//! de-duplicate and merge.
//!
//! Every path here is MOUNT-LOCAL — `main` nests the merged router under
//! `/api/crm`, and Core's ext-proxy rewrites the external `/api/ext/@ryu/crm/*`
//! onto the same prefix, so the two entry points serve byte-identical paths.
//! Writing `/api/crm/records/…` in this file would produce `/api/crm/api/crm/…`.
//!
//! Conventions this module does not get to re-decide:
//!
//! - **Paginated reads return the [`Page`] envelope at the top level** (`items` /
//!   `total` / `limit` / `offset` / `has_more`), never a bare array. The panel needs
//!   `total` to render "1–50 of 812" and cannot compute it from a page it already
//!   truncated.
//! - **Un-paginated list reads return `{"<plural>": [...]}`**, matching the sibling
//!   sidecars. `GET /records/:id/links` is the one route in this module in that
//!   shape: `{"links": [...]}`.
//! - **A write returns the entity (or the [`RecordUpdate`]) at the top level**, and a
//!   delete returns `{"ok": true, …}` rather than 204 — a bodiless response is
//!   indistinguishable from a dropped connection on the panel's side.
//! - **`limit` is clamped HERE, not in the store.** Every paginated store fn takes
//!   `limit`/`offset` as separate arguments and uses them verbatim; the fields on
//!   `RecordQuery`/`RelatedQuery`/`DuplicateScanRequest` are wire-only. Forwarding
//!   one raw compiles, looks right, and has no ceiling — `records.data` is an
//!   unbounded JSON blob per row.
//!
//! Handlers are thin. Validation, link projection, the automatic `field_change` /
//! `stage_change` timeline entries and the merge transaction all live in
//! [`crate::store`]; what is genuinely this file's job is (a) turning the store's
//! three-way `Validated` result into 404 / 422 / 200, (b) pre-checking the merge
//! cases the store `bail!`s on so they surface as 409 instead of 500, and (c)
//! raising the app events AFTER the write commits.

use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::events;
use crate::models::*;
use crate::state::AppState;

/// Build the record router.
///
/// Takes no state and returns `Router<AppState>`: `main` merges the six router
/// modules and calls `.with_state(state)` once. Calling `.with_state` here would
/// hand back a `Router<()>` that cannot be merged with its siblings.
///
/// The path parameters are spelled `:object` and `:record_id` because that is how
/// `objects.rs`, `views.rs` and `timeline.rs` spell the same positions. axum 0.7's
/// matcher treats two routes that differ only in parameter NAME at the same segment
/// as an insertion conflict and panics when the routers are merged — a failure that
/// shows up at boot in `main`, not as a compile error in this file.
pub fn routes() -> Router<AppState> {
    Router::new()
        // ── Per-object collections ──
        //
        // `/records/query` and `/records/validate` are static children of
        // `/records`, so they can never be captured as a record id.
        .route(
            "/objects/:object/records",
            get(list_records).post(create_record),
        )
        .route("/objects/:object/records/query", post(query_records))
        .route(
            "/objects/:object/records/validate",
            post(validate_record_values),
        )
        .route("/objects/:object/duplicates", post(scan_duplicates))
        // ── One record ──
        .route(
            "/records/:record_id",
            get(get_record).patch(patch_record).delete(delete_record),
        )
        .route("/records/:record_id/restore", post(restore_record))
        // ── Relations ──
        //
        // Unlink is a POST, not a DELETE: it carries a body (`target_record_ids`),
        // and a DELETE with a body is not reliably forwarded by every proxy in the
        // chain between the panel and this process.
        .route("/records/:record_id/links", get(list_links).post(link_records))
        .route("/records/:record_id/unlink", post(unlink_records))
        .route("/records/:record_id/related", get(related_records))
        // ── Merge ──
        //
        // Top-level, not under `/records/:record_id`, because a merge is about two or
        // more records at once and neither one is "the" subject of the URL. The
        // preview is declared alongside the apply so the pair stays visibly a pair.
        .route("/merge/preview", post(preview_merge))
        .route("/merge", post(apply_merge))
}

/// Alias for `routes`, in case `main` reaches for the other spelling. One of the two
/// is dead by construction, hence the allow.
#[allow(dead_code)]
pub fn router() -> Router<AppState> {
    routes()
}

// ── Shared helpers ─────────────────────────────────────────────────────────────

/// Resolve the `:object` path segment — an object id OR a slug — into the real row.
///
/// Done up front in every handler rather than left to the store: the store's
/// `query_records` answers an EMPTY PAGE for an unknown object (so a board with a
/// deleted column does not explode), which is the right behaviour there and the
/// wrong answer to `GET /objects/typo/records`. Resolving here also gets the handler
/// the `Object` it needs for the event payload for free.
async fn resolve_object(state: &AppState, id_or_slug: &str) -> ApiResult<Object> {
    state
        .store
        .get_object(id_or_slug)
        .await?
        .ok_or_else(|| ApiError::not_found("object"))
}

/// Raise the events one committed [`RecordUpdate`] implies.
///
/// Deliberately infallible. The write has already committed by the time this runs,
/// so a failure to look up the object for the payload must not turn a successful
/// PATCH into a 500 — the panel would show "save failed" for a row that saved. Emits
/// are best-effort by design and no-op entirely when the process is not Core-hosted.
///
/// `record.updated` is skipped when nothing moved: an empty `changed` means the PATCH
/// was a no-op, and a hook that re-reads the record on every event would otherwise
/// spin on idle cell edits.
async fn emit_record_update(state: &AppState, update: &RecordUpdate) {
    if update.changed.is_empty() && update.stage_change.is_none() {
        return;
    }
    let object = match state.store.get_object(&update.record.object_id).await {
        Ok(Some(object)) => object,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, record_id = %update.record.id, "record written but its event lookup failed");
            return;
        }
    };
    if !update.changed.is_empty() {
        events::record_updated(&state.events, &update.record, &object, &update.changed).await;
    }
    if let Some(stage) = &update.stage_change {
        events::deal_stage_changed(
            &state.events,
            &update.record,
            &object,
            &stage.field_id,
            &stage.field_slug,
            stage.from.as_deref(),
            stage.from_label.as_deref(),
            stage.to.as_deref(),
            stage.to_label.as_deref(),
        )
        .await;
    }
}

// ── Reads ──────────────────────────────────────────────────────────────────────

/// The query-string form of a record listing — what a plain table paint sends.
///
/// The full filter tree goes to `POST …/records/query` instead; expressing a nested
/// `and`/`or` in a query string is possible and nobody should have to read it.
#[derive(Debug, Default, Deserialize)]
struct RecordListQuery {
    #[serde(default)]
    search: Option<String>,
    /// A field id, a field slug, or one of the intrinsic keys (`title`,
    /// `created_at`, `updated_at`).
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    direction: Option<SortDirection>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
    /// The trash view sets this; everything else leaves soft-deleted rows hidden.
    #[serde(default)]
    include_deleted: bool,
    /// Restrict to the members of one curated list.
    #[serde(default)]
    list_id: Option<String>,
}

impl RecordListQuery {
    fn sorts(&self) -> Vec<ViewSort> {
        match self.sort.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(key) => vec![ViewSort {
                field_id: key.to_owned(),
                direction: self.direction.unwrap_or_default(),
            }],
            // Newest-touched first, matching every seeded view. With no sorts the
            // store falls back to `id ASC`, i.e. oldest first — on an object with a
            // year of history that opens the table on rows nobody is looking for.
            None => vec![ViewSort::desc("updated_at")],
        }
    }
}

fn non_empty(raw: Option<String>) -> Option<String> {
    raw.filter(|s| !s.trim().is_empty())
}

async fn list_records(
    State(state): State<AppState>,
    Path(object): Path<String>,
    Query(params): Query<RecordListQuery>,
) -> ApiResult<Json<RecordPage>> {
    let object = resolve_object(&state, &object).await?;
    let limit = state.config.clamp_limit(params.limit);
    let offset = params.offset.unwrap_or(0);
    let query = RecordQuery {
        object_id: object.id,
        filter: None,
        sorts: params.sorts(),
        search: non_empty(params.search),
        limit: params.limit,
        offset: params.offset,
        include_deleted: params.include_deleted,
        list_id: non_empty(params.list_id),
        record_ids: None,
    };
    Ok(Json(state.store.query_records(&query, limit, offset).await?))
}

/// The full query: the same filter/sort shape a [`View`] persists, so a saved view
/// and an ad-hoc table filter go down one code path in the store.
async fn query_records(
    State(state): State<AppState>,
    Path(object): Path<String>,
    Json(mut body): Json<RecordQuery>,
) -> ApiResult<Json<RecordPage>> {
    let object = resolve_object(&state, &object).await?;
    // The PATH wins over the body. `RecordQuery` carries its own `object_id` because
    // that is the shape the store takes; forwarding the body's copy would let
    // `POST /objects/company/records/query` with `{"object_id":"obj_deal"}` return
    // deals under a company URL — and every filter in the tree would then be resolved
    // against the wrong schema.
    body.object_id = object.id;
    let limit = state.config.clamp_limit(body.limit);
    let offset = body.offset.unwrap_or(0);
    Ok(Json(state.store.query_records(&body, limit, offset).await?))
}

#[derive(Debug, Default, Deserialize)]
struct DetailQuery {
    /// `?detail=true` returns the whole drawer — record, object, fields, both link
    /// directions, the newest 25 activities and list memberships — in one round trip.
    #[serde(default)]
    detail: bool,
}

async fn get_record(
    State(state): State<AppState>,
    Path(record_id): Path<String>,
    Query(params): Query<DetailQuery>,
) -> ApiResult<Json<Value>> {
    // Soft-deleted rows are returned, not hidden: the trash view opens them, and a
    // restore needs something to render first.
    if params.detail {
        let detail = state
            .store
            .get_record_detail(&record_id)
            .await?
            .ok_or_else(|| ApiError::not_found("record"))?;
        return Ok(Json(serde_json::to_value(detail)?));
    }
    let record = state
        .store
        .get_record(&record_id)
        .await?
        .ok_or_else(|| ApiError::not_found("record"))?;
    Ok(Json(serde_json::to_value(record)?))
}

// ── Writes ─────────────────────────────────────────────────────────────────────

async fn create_record(
    State(state): State<AppState>,
    Path(object): Path<String>,
    Json(body): Json<CreateRecordRequest>,
) -> ApiResult<Json<Record>> {
    let object = resolve_object(&state, &object).await?;
    let record = match state.store.create_record(&object.id, &body).await? {
        Ok(record) => record,
        // 422 with the full per-field list, not 400 with the first reason: a form
        // with four bad cells must light up four cells.
        Err(errors) => return Err(ApiError::validation(errors)),
    };
    events::record_created(&state.events, &record, &object).await;
    Ok(Json(record))
}

/// Patch a record's value bag.
///
/// Returns the whole [`RecordUpdate`] — the new row PLUS the diff — rather than the
/// row alone. The diff is what the panel animates and what tells it whether the save
/// was a no-op, and the store is the only place that has both bags in normalized form
/// at once, so recomputing it client-side is guesswork.
async fn patch_record(
    State(state): State<AppState>,
    Path(record_id): Path<String>,
    Json(body): Json<UpdateRecordRequest>,
) -> ApiResult<Json<RecordUpdate>> {
    let update = match state.store.update_record(&record_id, &body).await? {
        Ok(Some(update)) => update,
        Ok(None) => return Err(ApiError::not_found("record")),
        Err(errors) => return Err(ApiError::validation(errors)),
    };
    emit_record_update(&state, &update).await;
    Ok(Json(update))
}

#[derive(Debug, Default, Deserialize)]
struct DeleteQuery {
    /// `?purge=true` runs the irreversible cascade over FTS, links, list entries and
    /// activities. Opt-in per request, so the panel's ordinary delete cannot become
    /// one by accident.
    #[serde(default)]
    purge: bool,
}

async fn delete_record(
    State(state): State<AppState>,
    Path(record_id): Path<String>,
    Query(params): Query<DeleteQuery>,
) -> ApiResult<Json<Value>> {
    // Existence is resolved with `get_record` (which SEES soft-deleted rows) rather
    // than from the store's boolean: `delete_record` only reports `true` for a row it
    // actually moved, so mapping `false` to 404 would answer "not found" for an
    // already-trashed record that plainly exists. A double-click must be a no-op, not
    // an error toast.
    let record = state
        .store
        .get_record(&record_id)
        .await?
        .ok_or_else(|| ApiError::not_found("record"))?;
    if params.purge {
        state.store.purge_record(&record.id).await?;
        return Ok(Json(json!({ "ok": true, "purged": true })));
    }
    state.store.delete_record(&record.id).await?;
    Ok(Json(json!({ "ok": true, "purged": false })))
}

async fn restore_record(
    State(state): State<AppState>,
    Path(record_id): Path<String>,
) -> ApiResult<Json<Value>> {
    // Same asymmetry as the delete, mirrored: `restore_record` reports `false` for a
    // record that was never deleted, which is a successful no-op and not a 404.
    let record = state
        .store
        .get_record(&record_id)
        .await?
        .ok_or_else(|| ApiError::not_found("record"))?;
    state.store.restore_record(&record.id).await?;
    Ok(Json(json!({ "ok": true })))
}

/// Dry-run validation for an inline cell edit.
///
/// Always 200, even when every value was rejected — this route's whole purpose is to
/// hand back the error list so the panel can mark the cell BEFORE committing. A 422
/// here would make "this value is wrong" indistinguishable from "the request was
/// wrong" at the fetch layer.
#[derive(Debug, Default, Deserialize)]
struct ValidateValuesBody {
    #[serde(default)]
    values: ValueBag,
    /// `merge` (the default) validates only what was sent; `replace` also enforces
    /// required fields, matching what the corresponding PATCH would do.
    #[serde(default)]
    mode: UpdateMode,
    /// The record being edited. Excluded from the uniqueness check, so re-saving a
    /// person's own email does not collide with themselves.
    #[serde(default)]
    record_id: Option<String>,
}

async fn validate_record_values(
    State(state): State<AppState>,
    Path(object): Path<String>,
    Json(body): Json<ValidateValuesBody>,
) -> ApiResult<Json<ValidatedValues>> {
    let object = resolve_object(&state, &object).await?;
    Ok(Json(
        state
            .store
            .validate_values(
                &object.id,
                &body.values,
                body.mode == UpdateMode::Merge,
                body.record_id.as_deref(),
            )
            .await?,
    ))
}

// ── Relations ──────────────────────────────────────────────────────────────────

async fn list_links(
    State(state): State<AppState>,
    Path(record_id): Path<String>,
) -> ApiResult<Json<Value>> {
    if state.store.get_record(&record_id).await?.is_none() {
        return Err(ApiError::not_found("record"));
    }
    let links = state.store.list_links(&record_id).await?;
    Ok(Json(json!({ "links": links })))
}

async fn link_records(
    State(state): State<AppState>,
    Path(record_id): Path<String>,
    Json(body): Json<LinkRequest>,
) -> ApiResult<Json<RecordUpdate>> {
    mutate_links(&state, &record_id, &body, true).await
}

async fn unlink_records(
    State(state): State<AppState>,
    Path(record_id): Path<String>,
    Json(body): Json<LinkRequest>,
) -> ApiResult<Json<RecordUpdate>> {
    mutate_links(&state, &record_id, &body, false).await
}

/// Both edge mutations, so add and remove cannot drift in their error mapping.
///
/// The store writes the record's VALUE BAG and lets `record_links` follow — the bag
/// is authoritative — which is why this returns a [`RecordUpdate`] and emits
/// `record.updated` exactly like a PATCH. A relation edit IS a field edit.
async fn mutate_links(
    state: &AppState,
    record_id: &str,
    body: &LinkRequest,
    add: bool,
) -> ApiResult<Json<RecordUpdate>> {
    if body.field_id.trim().is_empty() {
        return Err(ApiError::bad_request(
            "field_id must name the relation field to change",
        ));
    }
    let result = if add {
        state.store.link_records(record_id, body).await?
    } else {
        state.store.unlink_records(record_id, body).await?
    };
    let update = match result {
        Ok(Some(update)) => update,
        Ok(None) => return Err(ApiError::not_found("record")),
        // A field that is not a relation on this object comes back as an
        // `unknown_field` rejection, so the panel can say WHICH field it got wrong.
        Err(errors) => return Err(ApiError::validation(errors)),
    };
    emit_record_update(state, &update).await;
    Ok(Json(update))
}

async fn related_records(
    State(state): State<AppState>,
    Path(record_id): Path<String>,
    Query(params): Query<RelatedQuery>,
) -> ApiResult<Json<RecordPage>> {
    // `related_records` answers an empty page for a record that does not exist, which
    // is indistinguishable from a record with no edges. Resolve first so the two read
    // differently.
    if state.store.get_record(&record_id).await?.is_none() {
        return Err(ApiError::not_found("record"));
    }
    let limit = state.config.clamp_limit(params.limit);
    let offset = params.offset.unwrap_or(0);
    Ok(Json(
        state
            .store
            .related_records(&record_id, &params, limit, offset)
            .await?,
    ))
}

// ── Dedupe + merge ─────────────────────────────────────────────────────────────

async fn scan_duplicates(
    State(state): State<AppState>,
    Path(object): Path<String>,
    Json(body): Json<DuplicateScanRequest>,
) -> ApiResult<Json<DuplicateScanResponse>> {
    let object = resolve_object(&state, &object).await?;
    let limit = state.config.clamp_limit(body.limit);
    // An empty `field_ids` means "decide for me" — the store picks unique fields,
    // then email fields, then the title field, and REPORTS which it used in the
    // response. A duplicate list the user cannot explain is one they will not act on.
    Ok(Json(
        state.store.merge_candidates(&object.id, &body, limit).await?,
    ))
}

/// Everything the store's `resolve_merge` would `bail!` on, checked before it sees
/// the plan.
///
/// A `bail!` becomes `ApiError::Internal`, i.e. a 500 with a fixed opaque message —
/// which is the wrong answer for four cases a user can trigger from the merge dialog
/// (no losers, merging a record into itself, records on different objects, naming
/// only ids that do not exist). Shared by the preview and the apply so the dry run
/// cannot accept a plan the apply then rejects.
async fn precheck_merge(state: &AppState, plan: &MergePlan) -> ApiResult<Record> {
    if plan.survivor_id.trim().is_empty() {
        return Err(ApiError::bad_request("a merge needs a survivor_id"));
    }
    let survivor = state
        .store
        .get_record(&plan.survivor_id)
        .await?
        .ok_or_else(|| ApiError::not_found("survivor record"))?;
    if plan.loser_ids.is_empty() {
        return Err(ApiError::conflict(
            "a merge needs at least one record to merge in",
        ));
    }
    let mut live = 0usize;
    for id in &plan.loser_ids {
        if id == &survivor.id {
            return Err(ApiError::conflict("a record cannot be merged into itself"));
        }
        // A loser id that resolves to nothing is skipped by the store rather than
        // failing the merge, so it is only fatal when NONE of them resolve.
        let Some(loser) = state.store.get_record(id).await? else {
            continue;
        };
        if loser.object_id != survivor.object_id {
            return Err(ApiError::conflict(
                "records on different objects cannot be merged",
            ));
        }
        live += 1;
    }
    if live == 0 {
        return Err(ApiError::conflict(
            "none of the named records to merge in exist",
        ));
    }
    Ok(survivor)
}

async fn preview_merge(
    State(state): State<AppState>,
    Json(plan): Json<MergePlan>,
) -> ApiResult<Json<MergePreview>> {
    precheck_merge(&state, &plan).await?;
    state
        .store
        .plan_merge(&plan)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("survivor record"))
}

async fn apply_merge(
    State(state): State<AppState>,
    Json(plan): Json<MergePlan>,
) -> ApiResult<Json<MergeOutcome>> {
    precheck_merge(&state, &plan).await?;
    let outcome = state
        .store
        .merge_records(&plan)
        .await?
        .ok_or_else(|| ApiError::not_found("survivor record"))?;

    // Emitted unconditionally, unlike a PATCH. A merge whose `changed` is empty still
    // retired records and re-parented their timeline, links and list memberships onto
    // the survivor — that is precisely what a downstream hook needs to hear, and it is
    // invisible in the value bag.
    match state.store.get_object(&outcome.survivor.object_id).await {
        Ok(Some(object)) => {
            events::record_updated(&state.events, &outcome.survivor, &object, &outcome.changed)
                .await;
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(%error, survivor_id = %outcome.survivor.id, "merge committed but its event lookup failed");
        }
    }
    Ok(Json(outcome))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState::in_memory().expect("an in-memory CRM opens and seeds")
    }

    fn bag(value: Value) -> ValueBag {
        value.as_object().cloned().expect("a JSON object")
    }

    /// Create through the HANDLER, not the store, so every test exercises the same
    /// resolve/validate/emit path a request takes.
    async fn make(state: &AppState, object: &str, values: Value) -> Record {
        create_record(
            State(state.clone()),
            Path(object.to_owned()),
            Json(CreateRecordRequest {
                values: bag(values),
                created_by: None,
            }),
        )
        .await
        .expect("a well-formed record is accepted")
        .0
    }

    fn plan(survivor: &str, losers: &[&str]) -> MergePlan {
        MergePlan {
            survivor_id: survivor.to_owned(),
            loser_ids: losers.iter().map(|s| (*s).to_owned()).collect(),
            resolutions: Vec::new(),
            // NOT `MergePlan::default()`: the derived `Default` gives
            // `soft_delete_losers = false`, while the serde default for a body that
            // omits the key is `true`. A test built on the derive would be testing the
            // hard-delete path while reading like the wire default.
            soft_delete_losers: true,
        }
    }

    // ── Validation ─────────────────────────────────────────────────────────────

    /// A slug the object does not have must be a 422 that NAMES the slug, not a
    /// silently dropped key. A CSV mapped one column off is otherwise a silent
    /// half-import.
    #[tokio::test]
    async fn an_unknown_slug_is_a_422_that_names_it() {
        let state = state();
        let err = create_record(
            State(state.clone()),
            Path("company".to_owned()),
            Json(CreateRecordRequest {
                values: bag(json!({ "name": "Acme", "revenoo": 12 })),
                created_by: None,
            }),
        )
        .await
        .expect_err("an unknown slug must not be accepted");

        match err {
            ApiError::Validation(errors) => assert!(
                errors
                    .iter()
                    .any(|e| e.field_slug == "revenoo" && e.code == ValidationCode::UnknownField),
                "the rejection must name the offending slug, got {errors:?}"
            ),
            other => panic!("expected a validation rejection, got {other:?}"),
        }
    }

    /// The dry-run route reports the SAME rejection with a 200, because the panel
    /// calls it to mark a cell before committing. A 422 here would be indistinguishable
    /// from a malformed request at the fetch layer.
    #[tokio::test]
    async fn dry_run_validation_reports_errors_without_failing() {
        let state = state();
        let out = validate_record_values(
            State(state.clone()),
            Path("person".to_owned()),
            Json(ValidateValuesBody {
                values: bag(json!({ "email": "not-an-email" })),
                mode: UpdateMode::Merge,
                record_id: None,
            }),
        )
        .await
        .expect("a dry run never fails on the values themselves")
        .0;

        assert!(!out.is_ok(), "a malformed email must be reported");
        assert!(out.errors.iter().any(|e| e.field_slug == "email"));
    }

    /// `merge` touches only what was sent; `replace` clears everything absent. Getting
    /// these the wrong way round silently wipes a record on every inline cell edit.
    #[tokio::test]
    async fn merge_mode_keeps_absent_fields_and_replace_clears_them() {
        let state = state();
        let record = make(
            &state,
            "company",
            json!({ "name": "Acme", "domain": "acme.test", "location": "Berlin" }),
        )
        .await;

        let merged = patch_record(
            State(state.clone()),
            Path(record.id.clone()),
            Json(UpdateRecordRequest {
                values: bag(json!({ "location": "Lisbon" })),
                mode: UpdateMode::Merge,
            }),
        )
        .await
        .expect("a merge patch is accepted")
        .0;
        assert_eq!(merged.record.values.get("domain"), Some(&json!("acme.test")));
        assert_eq!(merged.record.values.get("location"), Some(&json!("Lisbon")));
        assert_eq!(merged.changed.len(), 1, "only `location` moved");

        let replaced = patch_record(
            State(state.clone()),
            Path(record.id.clone()),
            Json(UpdateRecordRequest {
                values: bag(json!({ "name": "Acme", "location": "Lisbon" })),
                mode: UpdateMode::Replace,
            }),
        )
        .await
        .expect("a replace patch is accepted")
        .0;
        assert!(
            replaced.record.values.get("domain").is_none(),
            "replace must clear the fields it did not mention"
        );
    }

    /// In merge mode an explicit `null` CLEARS, while an absent slug is untouched.
    /// Without the distinction there is no way to empty a cell at all.
    #[tokio::test]
    async fn an_explicit_null_clears_in_merge_mode() {
        let state = state();
        let record = make(
            &state,
            "company",
            json!({ "name": "Acme", "location": "Berlin" }),
        )
        .await;

        let update = patch_record(
            State(state.clone()),
            Path(record.id),
            Json(UpdateRecordRequest {
                values: bag(json!({ "location": Value::Null })),
                mode: UpdateMode::Merge,
            }),
        )
        .await
        .expect("clearing a cell is a valid patch")
        .0;

        assert!(update.record.values.get("location").is_none());
        assert_eq!(update.changed.len(), 1);
    }

    // ── Pagination + scoping ───────────────────────────────────────────────────

    /// The path is the authority on which object is being queried. A body that names
    /// another object would resolve every filter in the tree against the wrong schema
    /// while the URL still reads as the right one.
    #[tokio::test]
    async fn a_query_body_cannot_override_the_path_object() {
        let state = state();
        make(&state, "company", json!({ "name": "Acme" })).await;
        make(&state, "person", json!({ "name": "Jane" })).await;

        let page = query_records(
            State(state.clone()),
            Path("company".to_owned()),
            Json(RecordQuery {
                object_id: OBJ_PERSON.to_owned(),
                ..RecordQuery::default()
            }),
        )
        .await
        .expect("the query runs")
        .0;

        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].object_id, OBJ_COMPANY);
    }

    /// `limit` is wire-only on `RecordQuery` — the store uses the separate argument
    /// verbatim. A handler that forwards `body.limit` compiles and has no ceiling.
    #[tokio::test]
    async fn a_huge_limit_is_clamped_to_the_configured_ceiling() {
        let state = state();
        make(&state, "company", json!({ "name": "Acme" })).await;

        let page = query_records(
            State(state.clone()),
            Path("company".to_owned()),
            Json(RecordQuery {
                limit: Some(1_000_000),
                ..RecordQuery::default()
            }),
        )
        .await
        .expect("the query runs")
        .0;

        assert_eq!(page.limit, crate::state::MAX_PAGE_SIZE);
    }

    /// The default listing sort is newest-touched first, not the store's `id ASC`
    /// fallback — an object with a year of history must not open on its oldest rows.
    #[tokio::test]
    async fn the_default_listing_sort_is_most_recently_updated_first() {
        let state = state();
        let first = make(&state, "company", json!({ "name": "Aaa" })).await;
        make(&state, "company", json!({ "name": "Bbb" })).await;

        // `updated_at` has millisecond resolution and the Windows system clock
        // ticks roughly every 15ms, so both creates AND the touch below can land
        // inside a single tick. The sort then ties, and `build_order_by`'s
        // `id ASC` pagination tie-break decides the order — by ULID entropy,
        // which is random within a millisecond rather than ordered by age. That
        // made this test fail on the windows-latest leg only, and only sometimes.
        // Wait for the clock to actually move so the assertion is about the sort
        // and not about which tick the test happened to run in.
        let mark = now_rfc3339();
        while now_rfc3339() == mark {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }

        patch_record(
            State(state.clone()),
            Path(first.id.clone()),
            Json(UpdateRecordRequest {
                values: bag(json!({ "location": "Berlin" })),
                mode: UpdateMode::Merge,
            }),
        )
        .await
        .expect("the touch lands");

        let page = list_records(
            State(state.clone()),
            Path("company".to_owned()),
            Query(RecordListQuery::default()),
        )
        .await
        .expect("the listing runs")
        .0;

        assert_eq!(page.items[0].id, first.id);
    }

    // ── Trash ──────────────────────────────────────────────────────────────────

    /// Delete and restore are idempotent, because the store's UPDATEs are guarded on
    /// the current state and report `false` for a row already in it. Mapping that
    /// `false` to 404 would answer "not found" for a record the panel is looking at.
    #[tokio::test]
    async fn deleting_and_restoring_twice_is_a_no_op_not_a_404() {
        let state = state();
        let record = make(&state, "company", json!({ "name": "Acme" })).await;

        for _ in 0..2 {
            delete_record(
                State(state.clone()),
                Path(record.id.clone()),
                Query(DeleteQuery { purge: false }),
            )
            .await
            .expect("a repeated delete is a successful no-op");
        }
        for _ in 0..2 {
            restore_record(State(state.clone()), Path(record.id.clone()))
                .await
                .expect("a repeated restore is a successful no-op");
        }

        let missing = restore_record(State(state.clone()), Path("rec_nope".to_owned()))
            .await
            .expect_err("a record that never existed is still a 404");
        assert!(matches!(missing, ApiError::NotFound(_)));
    }

    /// A soft delete hides the row from the default query but keeps it addressable,
    /// which is what makes the restore possible at all.
    #[tokio::test]
    async fn a_soft_deleted_record_is_hidden_but_still_fetchable() {
        let state = state();
        let record = make(&state, "company", json!({ "name": "Acme" })).await;
        delete_record(
            State(state.clone()),
            Path(record.id.clone()),
            Query(DeleteQuery { purge: false }),
        )
        .await
        .expect("the delete lands");

        let visible = list_records(
            State(state.clone()),
            Path("company".to_owned()),
            Query(RecordListQuery::default()),
        )
        .await
        .expect("the listing runs")
        .0;
        assert_eq!(visible.total, 0);

        let trashed = list_records(
            State(state.clone()),
            Path("company".to_owned()),
            Query(RecordListQuery {
                include_deleted: true,
                ..RecordListQuery::default()
            }),
        )
        .await
        .expect("the trash listing runs")
        .0;
        assert_eq!(trashed.total, 1);

        get_record(
            State(state.clone()),
            Path(record.id),
            Query(DetailQuery { detail: false }),
        )
        .await
        .expect("a trashed record is still addressable");
    }

    // ── Relations ──────────────────────────────────────────────────────────────

    /// Linking writes the value bag and the edge projection follows, so the link is
    /// visible from BOTH ends — the whole point of materialising `record_links`.
    #[tokio::test]
    async fn a_link_is_visible_from_both_ends_and_unlink_undoes_it() {
        let state = state();
        let company = make(&state, "company", json!({ "name": "Acme" })).await;
        let person = make(&state, "person", json!({ "name": "Jane" })).await;

        let update = link_records(
            State(state.clone()),
            Path(person.id.clone()),
            Json(LinkRequest {
                field_id: FLD_PERSON_COMPANY.to_owned(),
                target_record_ids: vec![company.id.clone()],
            }),
        )
        .await
        .expect("linking an existing company is accepted")
        .0;
        assert_eq!(update.changed.len(), 1, "a relation edit IS a field edit");

        let from_person = state.store.list_links(&person.id).await.unwrap();
        assert!(from_person
            .iter()
            .any(|l| l.record_id == company.id && l.direction == LinkDirection::Outgoing));

        let from_company = state.store.list_links(&company.id).await.unwrap();
        assert!(
            from_company
                .iter()
                .any(|l| l.record_id == person.id && l.direction == LinkDirection::Incoming),
            "the company must see the person without a mirrored row"
        );

        unlink_records(
            State(state.clone()),
            Path(person.id.clone()),
            Json(LinkRequest {
                field_id: FLD_PERSON_COMPANY.to_owned(),
                target_record_ids: vec![company.id.clone()],
            }),
        )
        .await
        .expect("unlinking is accepted");
        assert!(state.store.list_links(&company.id).await.unwrap().is_empty());
    }

    /// A non-relation field is a per-field 422, not a 500 — the panel must be able to
    /// say WHICH field it got wrong.
    #[tokio::test]
    async fn linking_through_a_non_relation_field_is_a_422() {
        let state = state();
        let person = make(&state, "person", json!({ "name": "Jane" })).await;

        let err = link_records(
            State(state.clone()),
            Path(person.id),
            Json(LinkRequest {
                field_id: FLD_PERSON_PHONE.to_owned(),
                target_record_ids: vec!["rec_whatever".to_owned()],
            }),
        )
        .await
        .expect_err("phone is not a relation");

        match err {
            ApiError::Validation(errors) => {
                assert_eq!(errors[0].code, ValidationCode::UnknownField);
            }
            other => panic!("expected a validation rejection, got {other:?}"),
        }
    }

    /// The store answers an empty page for a record that does not exist, which reads
    /// identically to a record with no edges. The handler must separate the two.
    #[tokio::test]
    async fn related_records_404s_rather_than_returning_an_empty_page() {
        let state = state();
        let err = related_records(
            State(state.clone()),
            Path("rec_nope".to_owned()),
            Query(RelatedQuery::default()),
        )
        .await
        .expect_err("an unknown record is a 404");
        assert!(matches!(err, ApiError::NotFound(_)));
    }

    // ── Dedupe ─────────────────────────────────────────────────────────────────

    /// With no fields named the scan must pick the object's `is_unique` fields first —
    /// on `person` that is `email` — and REPORT what it picked, because a duplicate
    /// list the user cannot explain is one they will not act on.
    #[tokio::test]
    async fn a_dedupe_scan_with_no_fields_picks_the_unique_field_and_says_so() {
        let state = state();
        // Uniqueness is enforced on WRITE, so a real duplicate can only be created by
        // trashing the first one — which is exactly how duplicates arise in practice
        // (an import, then a restore).
        let first = make(
            &state,
            "person",
            json!({ "name": "Jane Doe", "email": "jane@acme.test" }),
        )
        .await;
        delete_record(
            State(state.clone()),
            Path(first.id.clone()),
            Query(DeleteQuery { purge: false }),
        )
        .await
        .expect("the trash lands");
        make(
            &state,
            "person",
            json!({ "name": "Jane D", "email": "jane@acme.test" }),
        )
        .await;
        restore_record(State(state.clone()), Path(first.id.clone()))
            .await
            .expect("the restore lands");

        let scan = scan_duplicates(
            State(state.clone()),
            Path("person".to_owned()),
            Json(DuplicateScanRequest::default()),
        )
        .await
        .expect("the scan runs")
        .0;

        assert_eq!(
            scan.field_ids,
            vec![FLD_PERSON_EMAIL.to_owned()],
            "the scan must report which field it decided on"
        );
        let hit = scan
            .candidates
            .iter()
            .find(|c| c.value == "jane@acme.test")
            .expect("the shared email is a candidate");
        assert_eq!(hit.record_ids.len(), 2);
        assert!(hit.record_ids.contains(&first.id));
        let mut sorted = hit.record_ids.clone();
        sorted.sort();
        assert_eq!(
            hit.record_ids, sorted,
            "group_concat has no ordering guarantee, so the store sorts the ULID-ish \
             ids — that is what makes record_ids[0] the oldest, i.e. the survivor the \
             dialog suggests"
        );
    }

    // ── Merge ──────────────────────────────────────────────────────────────────

    /// Every case the store `bail!`s on is a case a user can trigger from the merge
    /// dialog, so each must be a 409 the panel can explain — never an opaque 500.
    #[tokio::test]
    async fn every_user_triggerable_merge_bail_is_a_409() {
        let state = state();
        let survivor = make(&state, "company", json!({ "name": "Acme" })).await;
        let company = make(&state, "company", json!({ "name": "Acme Inc" })).await;
        let person = make(&state, "person", json!({ "name": "Jane" })).await;

        let cases: Vec<MergePlan> = vec![
            plan(&survivor.id, &[]),
            plan(&survivor.id, &[survivor.id.as_str()]),
            plan(&survivor.id, &[person.id.as_str()]),
            plan(&survivor.id, &["rec_nope"]),
        ];
        for case in cases {
            let err = apply_merge(State(state.clone()), Json(case.clone()))
                .await
                .expect_err("the plan must be refused");
            assert!(
                matches!(err, ApiError::Conflict(_)),
                "expected a 409 for {case:?}, got {err:?}"
            );
            // The preview shares the pre-check, so it cannot accept a plan the apply
            // rejects.
            let previewed = preview_merge(State(state.clone()), Json(case.clone()))
                .await
                .expect_err("the preview must refuse it identically");
            assert!(matches!(previewed, ApiError::Conflict(_)));
        }

        let missing = apply_merge(
            State(state.clone()),
            Json(plan("rec_nope", &[company.id.as_str()])),
        )
        .await
        .expect_err("an absent survivor is a 404, not a 409");
        assert!(matches!(missing, ApiError::NotFound(_)));
    }

    /// The default resolution fills a blank on the survivor from a loser but NEVER
    /// overwrites a value the survivor already has — the single most-complained-about
    /// behaviour in every CRM that gets it wrong. A real disagreement is reported as a
    /// conflict for the dialog to resolve, not silently applied.
    #[tokio::test]
    async fn a_merge_fills_blanks_but_never_overwrites() {
        let state = state();
        let survivor = make(
            &state,
            "company",
            json!({ "name": "Acme", "location": "Berlin" }),
        )
        .await;
        let loser = make(
            &state,
            "company",
            json!({ "name": "Acme Inc", "location": "Lisbon", "domain": "acme.test" }),
        )
        .await;

        let preview = preview_merge(
            State(state.clone()),
            Json(plan(&survivor.id, &[loser.id.as_str()])),
        )
        .await
        .expect("the preview runs")
        .0;
        assert_eq!(
            preview.resolved_values.get("location"),
            Some(&json!("Berlin")),
            "a populated survivor field must survive untouched"
        );
        assert_eq!(
            preview.resolved_values.get("domain"),
            Some(&json!("acme.test")),
            "a blank survivor field must be filled from the loser"
        );
        assert!(
            preview
                .conflicts
                .iter()
                .any(|c| c.field_slug == "location"),
            "the disagreement must be reported for the dialog to resolve"
        );

        let outcome = apply_merge(
            State(state.clone()),
            Json(plan(&survivor.id, &[loser.id.as_str()])),
        )
        .await
        .expect("the merge runs")
        .0;
        assert_eq!(outcome.survivor.values.get("location"), Some(&json!("Berlin")));
        assert_eq!(
            outcome.survivor.values.get("domain"),
            Some(&json!("acme.test"))
        );
        assert!(
            outcome.changed.iter().any(|c| c.field_slug == "domain"),
            "the filled blank must appear in the diff the event is built from"
        );
    }

    /// An explicit resolution wins over the fill-blanks default, in both directions:
    /// taking a loser's value over a populated survivor, and pinning the survivor's.
    #[tokio::test]
    async fn an_explicit_resolution_overrides_the_default() {
        let state = state();
        let survivor = make(
            &state,
            "company",
            json!({ "name": "Acme", "location": "Berlin" }),
        )
        .await;
        let loser = make(
            &state,
            "company",
            json!({ "name": "Acme Inc", "location": "Lisbon" }),
        )
        .await;

        let mut chosen = plan(&survivor.id, &[loser.id.as_str()]);
        chosen.resolutions = vec![MergeFieldResolution {
            field_id: FLD_COMPANY_LOCATION.to_owned(),
            source: MergeSource::Loser {
                record_id: loser.id.clone(),
            },
        }];

        let outcome = apply_merge(State(state.clone()), Json(chosen))
            .await
            .expect("the merge runs")
            .0;
        assert_eq!(outcome.survivor.values.get("location"), Some(&json!("Lisbon")));
    }

    /// The whole point of a merge: the loser's history follows it onto the survivor
    /// before the loser is retired. A merge that dropped the timeline would lose the
    /// only record of why the survivor now says what it says.
    #[tokio::test]
    async fn a_merge_reparents_history_and_edges_then_retires_the_loser() {
        let state = state();
        let survivor = make(&state, "company", json!({ "name": "Acme" })).await;
        let loser = make(&state, "company", json!({ "name": "Acme Inc" })).await;
        let person = make(&state, "person", json!({ "name": "Jane" })).await;

        // One edge pointing AT the loser, and one note hanging off it.
        link_records(
            State(state.clone()),
            Path(person.id.clone()),
            Json(LinkRequest {
                field_id: FLD_PERSON_COMPANY.to_owned(),
                target_record_ids: vec![loser.id.clone()],
            }),
        )
        .await
        .expect("the link lands");
        state
            .store
            .create_activity(&CreateActivityRequest {
                record_id: Some(loser.id.clone()),
                kind: ActivityKind::Note,
                title: "Spoke at the conference".to_owned(),
                ..CreateActivityRequest::default()
            })
            .await
            .unwrap()
            .expect("the note is valid");

        let outcome = apply_merge(
            State(state.clone()),
            Json(plan(&survivor.id, &[loser.id.as_str()])),
        )
        .await
        .expect("the merge runs")
        .0;

        assert_eq!(outcome.merged_record_ids, vec![loser.id.clone()]);
        assert!(
            outcome.moved_activities >= 1,
            "the loser's note must follow it onto the survivor"
        );
        assert!(outcome.moved_links >= 1, "the edge must be re-pointed");

        let links = state.store.list_links(&survivor.id).await.unwrap();
        assert!(
            links.iter().any(|l| l.record_id == person.id),
            "the person must now point at the survivor"
        );

        // Soft-deleted, not gone: an unrecoverable merge is a support ticket.
        let retired = state
            .store
            .get_record(&loser.id)
            .await
            .unwrap()
            .expect("the loser row survives");
        assert!(retired.deleted_at.is_some());
    }
}
