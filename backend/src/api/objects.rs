//! Objects and fields — Harbor's SCHEMA surface.
//!
//! Every path here is relative to the mount (`main` nests the module routers under
//! `/api/crm`), so a path written as `/objects/:object` serves both the local mount
//! and Core's ext-proxy rewrite of `/api/ext/@ryu/crm/objects/:object`. Writing the
//! prefix in here would produce `/api/crm/api/crm/objects`.
//!
//! Response shapes follow the contract's declared types rather than `ryu-social`'s
//! `{"<plural>": [...]}` envelope. That envelope exists because social's companion
//! UI is a sandboxed manifest view DSL that names its row array by key; Harbor's
//! surface is a NATIVE dock panel typed against these structs, and the contract
//! specifies `Page<T>`, `SchemaResponse` and `#[serde(flatten)]`-ed summaries at the
//! top level throughout. So: lists are bare arrays, single entities are top level,
//! and a delete answers `{"ok": true}` (never 204 — a bodiless response is
//! indistinguishable from a dropped connection).
//!
//! ## What this module guards that the store does not
//!
//! The store `bail!`s on the two destructive edits the product forbids — deleting a
//! standard object, deleting a system field — which would surface as a 500. Both are
//! pre-checked here and answered 409, because "you asked for something this product
//! does not permit" is not the same fact as "the database broke".
//!
//! The larger one is option removal. `update_field` writes whatever config it is
//! handed; nothing downstream notices that a record still stores an option id the
//! field no longer defines. Such a value is invisible in the panel (no chip to
//! render), invisible to a board view (no column to group under), and silently
//! excluded from the pipeline report — a data loss that reports itself as nothing at
//! all. So [`patch_field`] counts the records still on every dropped option and
//! either refuses with those options named, or rewrites them from an explicit
//! `option_migration` map the caller sends alongside the config.

use std::collections::{BTreeMap, HashSet};

use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::models::*;
use crate::state::AppState;

/// How many records one `PATCH /fields/:id` will rewrite for an option removal.
///
/// A ceiling rather than an unbounded loop: each row costs one `update_record`,
/// which is its own transaction AND writes a `field_change` activity. Past this the
/// honest answer is "that is a bulk migration, not a form save" — the caller can
/// clear the values with a filtered bulk edit first, then drop the option.
const MAX_OPTION_MIGRATION_RECORDS: i64 = 10_000;

/// Rows fetched per migration pass. Deliberately modest: every row in the page is
/// rewritten before the next page is read, so a large page buys nothing.
const MIGRATION_PAGE: usize = 200;

/// Build the schema router. `/health` is NOT here — it must sit outside the auth
/// gate, so `main` owns it.
pub fn routes() -> Router<AppState> {
    Router::new()
        // The panel's boot call: every object with its fields, views and counts.
        .route("/schema", get(schema))
        // ── Objects ──
        .route("/objects", get(list_objects).post(create_object))
        .route(
            "/objects/:object",
            get(get_object).patch(patch_object).delete(delete_object),
        )
        // ── Fields ──
        //
        // `/objects/:object/fields/reorder` is a deeper path than
        // `/objects/:object/fields`, so the two cannot collide regardless of
        // insertion order — and each is a SEPARATE `http.routes[]` entry in the
        // manifest, because Core's ext-proxy matcher requires an exact segment-count
        // match and declaring the parent does not admit the child.
        .route(
            "/objects/:object/fields",
            get(list_fields).post(create_field),
        )
        .route("/objects/:object/fields/reorder", post(reorder_fields))
        // Fields are addressed by id at the top level, not under their object: every
        // client that holds a field already holds its id, and threading the object
        // through would let the two disagree.
        .route(
            "/fields/:field_id",
            get(get_field).patch(patch_field).delete(delete_field),
        )
}

/// The name the task brief uses for the mount point. `routes()` is the contract's
/// name and the one the other five router modules export; this alias exists so
/// `main` compiles against either without a later edit racing another job's file.
#[allow(dead_code)]
pub fn router() -> Router<AppState> {
    routes()
}

// ── Shared helpers ─────────────────────────────────────────────────────────────

/// Turn a store `bool` "did anything change" into a 404, so a caller can tell a
/// missing row from a successful no-op.
fn require_hit(changed: bool, what: &str) -> ApiResult<()> {
    if changed {
        Ok(())
    } else {
        Err(ApiError::not_found(what))
    }
}

/// Resolve the `:object` path segment (an id OR a slug) into the real row.
///
/// Called before every store write rather than passing the raw segment through:
/// most store fns resolve id-or-slug themselves, but `reorder_fields` does NOT — it
/// uses the argument verbatim in `WHERE object_id = ?1`, so handing it a slug would
/// reposition the listed fields while leaving every unlisted one where it was, and
/// the collision is silent.
async fn resolve_object(state: &AppState, id_or_slug: &str) -> ApiResult<Object> {
    state
        .store
        .get_object(id_or_slug)
        .await?
        .ok_or_else(|| ApiError::not_found("object"))
}

// ── Schema ─────────────────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/crm/schema",
    tag = "CRM",
    summary = "read the whole CRM schema in one call — every object with its fields, views and record counts. Start here to learn what objects and field slugs this node actually has.",
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn schema(State(state): State<AppState>) -> ApiResult<Json<SchemaResponse>> {
    Ok(Json(state.store.schema().await?))
}

// ── Objects ────────────────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/crm/objects",
    tag = "CRM",
    summary = "list the CRM's objects (companies, people, deals, and any custom ones) with their record counts.",
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn list_objects(
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<ObjectSummary>>> {
    Ok(Json(state.store.list_object_summaries().await?))
}

#[utoipa::path(
    post,
    path = "/api/crm/objects",
    tag = "CRM",
    summary = "define a new custom object — a new kind of record this CRM tracks.",
    request_body = CreateObjectRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn create_object(
    State(state): State<AppState>,
    Json(body): Json<CreateObjectRequest>,
) -> ApiResult<Json<Object>> {
    match state.store.create_object(&body).await? {
        Ok(object) => Ok(Json(object)),
        Err(errors) => Err(ApiError::validation(errors)),
    }
}

#[utoipa::path(
    get,
    path = "/api/crm/objects/{object}",
    tag = "CRM",
    summary = "read one object's definition by id or slug.",
    params(("object" = String, Path, description = "Object id or slug, e.g. `company`, `person`, `deal`.")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn get_object(
    State(state): State<AppState>,
    Path(object): Path<String>,
) -> ApiResult<Json<Object>> {
    Ok(Json(resolve_object(&state, &object).await?))
}

#[utoipa::path(
    patch,
    path = "/api/crm/objects/{object}",
    tag = "CRM",
    summary = "rename an object, change its icon, or pick which field titles its records.",
    params(("object" = String, Path, description = "Object id or slug.")),
    request_body = UpdateObjectRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn patch_object(
    State(state): State<AppState>,
    Path(object): Path<String>,
    Json(body): Json<UpdateObjectRequest>,
) -> ApiResult<Json<Object>> {
    let existing = resolve_object(&state, &object).await?;

    // A `title_field_id` naming a field on some OTHER object is not a validation
    // nicety: `Record::title` is derived from it on every write, so a wrong id
    // silently retitles every record in the object to a blank.
    if let Some(title) = body
        .title_field_id
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        if state
            .store
            .resolve_field(&existing.id, title)
            .await?
            .is_none()
        {
            return Err(ApiError::bad_request(format!(
                "no field \"{title}\" on the \"{}\" object",
                existing.slug
            )));
        }
    }

    state
        .store
        .update_object(&existing.id, &body)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("object"))
}

#[utoipa::path(
    delete,
    path = "/api/crm/objects/{object}",
    tag = "CRM",
    summary = "delete a custom object and everything in it. The five standard objects cannot be deleted.",
    params(("object" = String, Path, description = "Object id or slug.")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn delete_object(
    State(state): State<AppState>,
    Path(object): Path<String>,
) -> ApiResult<Json<Value>> {
    let existing = resolve_object(&state, &object).await?;
    // Pre-checked HERE rather than by mapping the store's `bail!` to a 409: that
    // mapping would also report a genuine SQL failure as "not deletable" and send
    // whoever debugs it down the wrong path.
    if existing.is_standard {
        return Err(ApiError::conflict(format!(
            "\"{}\" is a standard object the product depends on and cannot be deleted",
            existing.singular
        )));
    }
    require_hit(state.store.delete_object(&existing.id).await?, "object")?;
    Ok(Json(json!({ "ok": true })))
}

// ── Fields ─────────────────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/crm/objects/{object}/fields",
    tag = "CRM",
    summary = "list one object's fields with their types, slugs and select options — how to learn the field slugs a record write needs.",
    params(("object" = String, Path, description = "Object id or slug.")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn list_fields(
    State(state): State<AppState>,
    Path(object): Path<String>,
) -> ApiResult<Json<Vec<Field>>> {
    // Resolved first so an unknown object is a 404 rather than an empty list — the
    // store returns `Vec::new()` for both, and a panel cannot tell "this object has
    // no fields" from "this object does not exist".
    let existing = resolve_object(&state, &object).await?;
    Ok(Json(state.store.list_fields(&existing.id).await?))
}

#[utoipa::path(
    post,
    path = "/api/crm/objects/{object}/fields",
    tag = "CRM",
    summary = "add a typed field to an object — text, number, currency, date, select, status, relation, and so on.",
    params(("object" = String, Path, description = "Object id or slug.")),
    request_body = CreateFieldRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn create_field(
    State(state): State<AppState>,
    Path(object): Path<String>,
    Json(body): Json<CreateFieldRequest>,
) -> ApiResult<Json<Field>> {
    // `create_field` `bail!`s (→ 500) on an unknown object; resolving first turns
    // that into the 404 it actually is.
    let existing = resolve_object(&state, &object).await?;
    match state.store.create_field(&existing.id, None, &body).await? {
        Ok(field) => Ok(Json(field)),
        Err(errors) => Err(ApiError::validation(errors)),
    }
}

#[utoipa::path(
    post,
    path = "/api/crm/objects/{object}/fields/reorder",
    tag = "CRM",
    summary = "set the display order of an object's fields.",
    params(("object" = String, Path, description = "Object id or slug.")),
    request_body = ReorderRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn reorder_fields(
    State(state): State<AppState>,
    Path(object): Path<String>,
    Json(body): Json<ReorderRequest>,
) -> ApiResult<Json<Vec<Field>>> {
    let existing = resolve_object(&state, &object).await?;
    let fields = state.store.list_fields(&existing.id).await?;

    // `reorder_fields` writes `UPDATE fields SET position = ? WHERE id = ?` with no
    // object scope, so an id belonging to another object would be repositioned into
    // THIS object's ordering and corrupt both. A duplicate id is rejected for the
    // same reason the ids are checked at all: the last occurrence would win and the
    // caller would get an order it did not ask for, with no error.
    let mut seen = HashSet::with_capacity(body.ids.len());
    for id in &body.ids {
        if !fields.iter().any(|f| &f.id == id) {
            return Err(ApiError::bad_request(format!(
                "\"{id}\" is not a field of the \"{}\" object",
                existing.slug
            )));
        }
        if !seen.insert(id.as_str()) {
            return Err(ApiError::bad_request(format!(
                "field \"{id}\" is listed twice"
            )));
        }
    }

    state
        .store
        .reorder_fields(&existing.id, None, &body.ids)
        .await?;
    Ok(Json(state.store.list_fields(&existing.id).await?))
}

#[utoipa::path(
    get,
    path = "/api/crm/fields/{field_id}",
    tag = "CRM",
    summary = "read one field's definition, including its select options.",
    params(("field_id" = String, Path, description = "The field's id.")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn get_field(
    State(state): State<AppState>,
    Path(field_id): Path<String>,
) -> ApiResult<Json<Field>> {
    state
        .store
        .get_field(&field_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("field"))
}

#[utoipa::path(
    delete,
    path = "/api/crm/fields/{field_id}",
    tag = "CRM",
    summary = "delete a field and every value recorded in it. System fields cannot be deleted.",
    params(("field_id" = String, Path, description = "The field's id.")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn delete_field(
    State(state): State<AppState>,
    Path(field_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let field = state
        .store
        .get_field(&field_id)
        .await?
        .ok_or_else(|| ApiError::not_found("field"))?;

    if field.is_system {
        return Err(ApiError::conflict(format!(
            "\"{}\" is a system field and cannot be deleted",
            field.name
        )));
    }

    // The store clears a view's `group_by_field_id` when its field goes, but nothing
    // clears an object's `title_field_id`. A dangling one makes every record on the
    // object fall back to a blank title on its next write, which reads as data loss.
    // Only reachable when the user re-pointed the title at a non-system field, since
    // the seeded and auto-created title fields are all `is_system`.
    if let Some(object) = state.store.get_object(&field.object_id).await? {
        if object.title_field_id.as_deref() == Some(field.id.as_str()) {
            return Err(ApiError::conflict(format!(
                "\"{}\" is the title field of the \"{}\" object — point the title at another field first",
                field.name, object.singular
            )));
        }
    }

    require_hit(state.store.delete_field(&field.id).await?, "field")?;
    Ok(Json(json!({ "ok": true })))
}

// ── PATCH /fields/:field_id, and the option-removal problem ────────────────────

/// The `PATCH /fields/:field_id` body: an [`UpdateFieldRequest`] plus the ONE extra
/// key that makes dropping an option safe.
///
/// Flattened rather than nested so the common case — renaming a field, adding an
/// option, repositioning — is exactly the request shape the contract documents, and
/// `option_migration` only appears when the caller is removing something.
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct PatchFieldBody {
    #[serde(flatten)]
    pub field: UpdateFieldRequest,
    /// Removed option id → what the records still holding it become: another option
    /// (named by id OR by label, resolved against the config being written) or
    /// `null` to clear those cells.
    ///
    /// Only consulted for options that are BOTH removed and still in use. A key
    /// naming an option that is not being removed is rejected rather than ignored —
    /// it means the caller and the server disagree about what this request does.
    #[serde(default)]
    pub option_migration: BTreeMap<String, Option<String>>,
}

/// What a config edit must do to the records referencing options it drops.
#[derive(Debug, Default)]
struct OptionMigration {
    /// Dropped option id → the caller's raw replacement (`None` = clear). Holds only
    /// options that are actually still in use; a dropped option nothing references
    /// needs no plan and no permission.
    remap: BTreeMap<String, Option<String>>,
    /// Whether every replacement ALREADY exists in the field's current config, so
    /// the records can be rewritten BEFORE the config is written.
    ///
    /// This is the difference between the two failure modes. Rewrite-first: a
    /// mid-loop failure leaves some records on the new option and the config
    /// untouched — every value is still valid and the whole request is retryable.
    /// Config-first (unavoidable when the replacement is an option this same request
    /// is ADDING, since it has no id until the store assigns one): a mid-loop failure
    /// leaves the rest of the records orphaned, which is precisely what this guard
    /// exists to prevent.
    rewrite_first: bool,
}

#[utoipa::path(
    patch,
    path = "/api/crm/fields/{field_id}",
    tag = "CRM",
    summary = "rename a field, edit its options, or change its requirements. A field's slug and type are immutable.",
    params(("field_id" = String, Path, description = "The field's id.")),
    request_body = PatchFieldBody,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn patch_field(
    State(state): State<AppState>,
    Path(field_id): Path<String>,
    Json(body): Json<PatchFieldBody>,
) -> ApiResult<Json<Value>> {
    let existing = state
        .store
        .get_field(&field_id)
        .await?
        .ok_or_else(|| ApiError::not_found("field"))?;

    // A system field's `name`, `config` and `position` are the user's; its FLAGS are
    // the product's. Compared against the current value rather than rejected on
    // presence, so a panel that PATCHes the whole field back unchanged still works —
    // only an actual flip is refused.
    if existing.is_system {
        if body
            .field
            .is_required
            .is_some_and(|next| next != existing.is_required)
        {
            return Err(ApiError::conflict(format!(
                "\"{}\" is a system field: its required flag is part of the object's shape and cannot be changed",
                existing.name
            )));
        }
        if body
            .field
            .is_unique
            .is_some_and(|next| next != existing.is_unique)
        {
            return Err(ApiError::conflict(format!(
                "\"{}\" is a system field: its uniqueness cannot be changed",
                existing.name
            )));
        }
    }

    let plan = match body.field.config.as_ref() {
        Some(incoming) => {
            plan_option_migration(&state, &existing, incoming, &body.option_migration).await?
        }
        // No config in the request means the store keeps the existing one, so nothing
        // can be dropped — but a stray `option_migration` still means the caller
        // thinks it is dropping something.
        None => {
            reject_unknown_migration_keys(&body.option_migration, &HashSet::new(), &existing)?;
            OptionMigration::default()
        }
    };

    let mut migrated = 0usize;
    if plan.rewrite_first {
        migrated = apply_option_migration(&state, &existing, &plan).await?;
    }

    let updated = match state.store.update_field(&existing.id, &body.field).await? {
        Ok(Some(field)) => field,
        Ok(None) => return Err(ApiError::not_found("field")),
        Err(errors) => return Err(ApiError::validation(errors)),
    };

    if !plan.rewrite_first {
        migrated = apply_option_migration(&state, &updated, &plan).await?;
    }

    // The field at the TOP level (so a client binds `name`/`config` exactly as it
    // does on every other field route) plus one extra key, the same inlining shape
    // `ObjectSummary` uses. Always present, including as `0`: a key that appears only
    // sometimes is the kind a client reads once and then crashes on.
    let mut out = serde_json::to_value(&updated)?;
    out["migrated_records"] = json!(migrated);
    Ok(Json(out))
}

/// Decide what a config edit does to the records on the options it drops: nothing,
/// a refusal, or a rewrite plan.
///
/// Returns `Ok(default)` for every non-option-backed field and for a config edit that
/// drops nothing, which is the overwhelmingly common PATCH.
async fn plan_option_migration(
    state: &AppState,
    field: &Field,
    incoming: &FieldConfig,
    requested: &BTreeMap<String, Option<String>>,
) -> ApiResult<OptionMigration> {
    if !field.field_type.is_option_backed() {
        reject_unknown_migration_keys(requested, &HashSet::new(), field)?;
        return Ok(OptionMigration::default());
    }

    // An option the request sends WITHOUT an id is a new one — the store assigns it a
    // deterministic id on write. It cannot be a "kept" id, and it is never a removal.
    let kept: HashSet<&str> = incoming
        .options
        .iter()
        .map(|option| option.id.as_str())
        .filter(|id| !id.is_empty())
        .collect();
    let dropped: Vec<&SelectOption> = field
        .config
        .options
        .iter()
        .filter(|option| !kept.contains(option.id.as_str()))
        .collect();
    let dropped_ids: HashSet<&str> = dropped.iter().map(|option| option.id.as_str()).collect();
    reject_unknown_migration_keys(requested, &dropped_ids, field)?;

    if dropped.is_empty() {
        return Ok(OptionMigration::default());
    }

    let mut plan = OptionMigration::default();
    let mut orphaning: Vec<String> = Vec::new();
    let mut affected: i64 = 0;

    for option in dropped {
        let in_use = count_option_usage(state, field, &option.id).await?;
        if in_use == 0 {
            // Nothing references it — removing it is a pure schema edit.
            continue;
        }
        affected += in_use;
        match requested.get(&option.id) {
            None => orphaning.push(format!("\"{}\" ({in_use})", option.label)),
            Some(target) => {
                if let Some(raw) = target {
                    let replacement = incoming.resolve_option(raw).ok_or_else(|| {
                        ApiError::bad_request(format!(
                            "cannot migrate \"{}\" to \"{raw}\": the configuration being written has no such option",
                            option.label
                        ))
                    })?;
                    // Migrating an option onto itself, or onto another option this
                    // request also removes, would leave the value exactly as
                    // orphaned as doing nothing — and the rewrite loop would spin,
                    // because the row never leaves the filter.
                    if dropped_ids.contains(replacement.id.as_str()) {
                        return Err(ApiError::bad_request(format!(
                            "cannot migrate \"{}\" to \"{}\": that option is being removed by this same request",
                            option.label, replacement.label
                        )));
                    }
                }
                plan.remap.insert(option.id.clone(), target.clone());
            }
        }
    }

    if !orphaning.is_empty() {
        return Err(ApiError::conflict(format!(
            "removing {} would orphan the values of {} record(s): {}. Send `option_migration` mapping each one to a replacement option, or to null to clear those cells.",
            if orphaning.len() == 1 { "that option" } else { "those options" },
            affected,
            orphaning.join(", ")
        )));
    }

    if affected > MAX_OPTION_MIGRATION_RECORDS {
        return Err(ApiError::conflict(format!(
            "{affected} records still use the options being removed, over the {MAX_OPTION_MIGRATION_RECORDS} this endpoint rewrites in one request — clear those values in bulk first, then remove the options"
        )));
    }

    plan.rewrite_first = plan.remap.values().all(|target| match target {
        // A clear is always safe to do first: the cell ends up empty either way.
        None => true,
        Some(raw) => incoming.resolve_option(raw).is_some_and(|option| {
            !option.id.is_empty() && field.config.option(&option.id).is_some()
        }),
    });

    Ok(plan)
}

/// A migration key that names something this request is not removing is a caller/
/// server disagreement about what the request does, so it is an error rather than a
/// silently ignored key.
fn reject_unknown_migration_keys(
    requested: &BTreeMap<String, Option<String>>,
    dropped_ids: &HashSet<&str>,
    field: &Field,
) -> ApiResult<()> {
    for key in requested.keys() {
        if !dropped_ids.contains(key.as_str()) {
            return Err(ApiError::bad_request(format!(
                "`option_migration` names \"{key}\", which \"{}\" is not removing",
                field.name
            )));
        }
    }
    Ok(())
}

/// How many rows still store this option id.
///
/// Counted with `include_deleted`, because a soft-deleted record is one restore away
/// from being live again — skipping them would let "delete the record, drop the
/// option, restore the record" resurrect a value pointing at an option that no longer
/// exists, which is exactly the state this guard exists to make unreachable.
///
/// A LIST-specific field's values live in `list_entries.data`, which `count_records`
/// cannot see at all; that branch goes through the list-entry query, whose filter
/// binds a list field to the entry rather than the record.
async fn count_option_usage(state: &AppState, field: &Field, option_id: &str) -> ApiResult<i64> {
    // `is_any_of` rather than `eq` so the ONE condition covers both shapes: the store
    // compiles it to `IN (…)` for a scalar select/status and to a `json_each`
    // membership test for a multi_select array.
    let filter = ViewFilter::Condition(FilterCondition {
        field_id: field.id.clone(),
        op: FilterOperator::IsAnyOf,
        value: json!([option_id]),
    });
    match field.list_id.as_deref() {
        Some(list_id) => {
            let page = state
                .store
                .query_list_entries(
                    &ListEntryQuery {
                        list_id: list_id.to_string(),
                        filter: Some(filter),
                        ..Default::default()
                    },
                    1,
                    0,
                )
                .await?;
            Ok(page.total)
        }
        None => Ok(state
            .store
            .count_records(&RecordQuery {
                object_id: field.object_id.clone(),
                filter: Some(filter),
                include_deleted: true,
                ..Default::default()
            })
            .await?),
    }
}

/// Rewrite every row still holding a dropped option, according to `plan`.
///
/// `field` is whichever version of the field the plan resolves against — the current
/// one when `rewrite_first`, the freshly written one otherwise. Its config is what
/// turns a raw replacement (which may have been a LABEL) into the option id the
/// records will store.
async fn apply_option_migration(
    state: &AppState,
    field: &Field,
    plan: &OptionMigration,
) -> ApiResult<usize> {
    if plan.remap.is_empty() {
        return Ok(0);
    }

    let mut resolved: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (old, target) in &plan.remap {
        let next = match target {
            None => None,
            Some(raw) => Some(
                field
                    .config
                    .resolve_option(raw)
                    .map(|option| option.id.clone())
                    .ok_or_else(|| {
                        ApiError::bad_request(format!(
                            "cannot migrate \"{old}\" to \"{raw}\": no such option on \"{}\"",
                            field.name
                        ))
                    })?,
            ),
        };
        resolved.insert(old.clone(), next);
    }

    let filter = ViewFilter::Condition(FilterCondition {
        field_id: field.id.clone(),
        op: FilterOperator::IsAnyOf,
        value: Value::Array(resolved.keys().map(|id| json!(id)).collect()),
    });

    let mut migrated = 0usize;
    loop {
        // Always page from offset 0. A rewritten row no longer matches the filter, so
        // the next page is what is LEFT, not the next window — walking the offset
        // forward would skip a page's worth of rows on every pass.
        let (batch, remaining) = load_migration_batch(state, field, &filter).await?;
        if batch.is_empty() {
            break;
        }

        let mut progressed = 0usize;
        for row in &batch {
            let next = migrate_option_value(row.values.get(&field.slug), &resolved);
            let mut values = ValueBag::new();
            values.insert(field.slug.clone(), next);
            if write_migrated_row(state, field, &row.id, values).await? {
                progressed += 1;
            }
        }
        migrated += progressed;

        // Forward-progress assertion. Without it, any row that matches the filter but
        // cannot be moved off it turns this into an infinite loop holding a request
        // open forever — a far worse failure than the 500 this returns.
        if progressed == 0 {
            return Err(ApiError::from(anyhow::anyhow!(
                "option migration on field {} stalled with {remaining} row(s) still referencing a removed option",
                field.id
            )));
        }
        if migrated as i64 > MAX_OPTION_MIGRATION_RECORDS {
            return Err(ApiError::conflict(
                "more records referenced the removed options than this endpoint rewrites in one request",
            ));
        }
    }

    Ok(migrated)
}

/// One page of rows to rewrite, reduced to the two things the loop needs: the row's
/// id and its current value bag. Erases the record/list-entry split so the loop above
/// does not branch.
struct MigrationRow {
    /// A record id, or a LIST ENTRY id when the field is list-specific.
    id: String,
    values: ValueBag,
}

async fn load_migration_batch(
    state: &AppState,
    field: &Field,
    filter: &ViewFilter,
) -> ApiResult<(Vec<MigrationRow>, i64)> {
    match field.list_id.as_deref() {
        Some(list_id) => {
            let page = state
                .store
                .query_list_entries(
                    &ListEntryQuery {
                        list_id: list_id.to_string(),
                        filter: Some(filter.clone()),
                        ..Default::default()
                    },
                    MIGRATION_PAGE,
                    0,
                )
                .await?;
            let total = page.total;
            let rows = page
                .items
                .into_iter()
                .map(|view| MigrationRow {
                    id: view.entry.id,
                    values: view.entry.values,
                })
                .collect();
            Ok((rows, total))
        }
        None => {
            let page = state
                .store
                .query_records(
                    &RecordQuery {
                        object_id: field.object_id.clone(),
                        filter: Some(filter.clone()),
                        include_deleted: true,
                        ..Default::default()
                    },
                    MIGRATION_PAGE,
                    0,
                )
                .await?;
            let total = page.total;
            let rows = page
                .items
                .into_iter()
                .map(|record| MigrationRow {
                    id: record.id,
                    values: record.values,
                })
                .collect();
            Ok((rows, total))
        }
    }
}

/// Write one migrated row. `Ok(false)` means the row did not move — the caller's
/// forward-progress check turns a whole page of those into an error rather than a
/// spin.
///
/// A per-row validation rejection aborts the WHOLE migration as a 422 instead of
/// being skipped: skipping leaves that row on the filter forever, and the loop would
/// re-read it on the next pass.
async fn write_migrated_row(
    state: &AppState,
    field: &Field,
    row_id: &str,
    values: ValueBag,
) -> ApiResult<bool> {
    if field.list_id.is_some() {
        return match state
            .store
            .update_list_entry(
                row_id,
                &UpdateListEntryRequest {
                    values,
                    mode: UpdateMode::Merge,
                },
            )
            .await?
        {
            // The store returns the entry, not a diff, so "it wrote" is the best
            // signal available; the loop's own re-query is what proves it moved.
            Ok(Some(_)) => Ok(true),
            // Removed from the list mid-migration: gone from the filter either way.
            Ok(None) => Ok(true),
            Err(errors) => Err(ApiError::validation(errors)),
        };
    }

    match state
        .store
        .update_record(
            row_id,
            &UpdateRecordRequest {
                values,
                mode: UpdateMode::Merge,
            },
        )
        .await?
    {
        Ok(Some(update)) => Ok(!update.changed.is_empty()),
        Ok(None) => Ok(true),
        Err(errors) => Err(ApiError::validation(errors)),
    }
}

/// What one cell becomes under a remap. PURE — the whole reason the loop above can be
/// trusted is that this decision is testable without a database.
///
/// `resolved` maps a REMOVED option id to its replacement id (`None` = clear). An id
/// absent from the map is an option that survives the edit and is left alone, which
/// is what makes a multi_select holding one dropped and one kept option come out with
/// the kept one intact.
fn migrate_option_value(
    current: Option<&Value>,
    resolved: &BTreeMap<String, Option<String>>,
) -> Value {
    match current {
        // multi_select: rewrite element-wise. Deduplicated, because migrating A→B on
        // a record already holding B would otherwise store B twice and every consumer
        // would double-count it.
        Some(Value::Array(items)) => {
            let mut out: Vec<Value> = Vec::with_capacity(items.len());
            for item in items {
                let Some(id) = item.as_str() else { continue };
                let next = match resolved.get(id) {
                    None => Some(id.to_string()),
                    Some(Some(replacement)) => Some(replacement.clone()),
                    Some(None) => None,
                };
                let Some(next) = next else { continue };
                if !out.iter().any(|kept| kept.as_str() == Some(next.as_str())) {
                    out.push(Value::String(next));
                }
            }
            // An emptied array clears the cell rather than storing `[]`: the store's
            // "is this set" test treats both as empty, and one representation is
            // easier to reason about than two.
            if out.is_empty() {
                Value::Null
            } else {
                Value::Array(out)
            }
        }
        Some(Value::String(id)) => match resolved.get(id.as_str()) {
            Some(Some(replacement)) => Value::String(replacement.clone()),
            Some(None) => Value::Null,
            None => Value::String(id.clone()),
        },
        // Nothing recognisable is there — clearing is the only safe answer, and the
        // row matched the filter so it must be moved off it.
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState::in_memory().expect("in-memory state")
    }

    fn bag(pairs: &[(&str, Value)]) -> ValueBag {
        let mut out = ValueBag::new();
        for (key, value) in pairs {
            out.insert((*key).to_string(), value.clone());
        }
        out
    }

    fn remap(pairs: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(old, new)| ((*old).to_string(), new.map(str::to_string)))
            .collect()
    }

    async fn company(state: &AppState, name: &str, status: &str) -> Record {
        state
            .store
            .create_record(
                "obj_company",
                &CreateRecordRequest {
                    values: bag(&[("name", json!(name)), ("status", json!(status))]),
                    created_by: None,
                },
            )
            .await
            .expect("create company")
            .expect("company values validate")
    }

    async fn status_field(state: &AppState) -> Field {
        state
            .store
            .get_field("fld_company_status")
            .await
            .expect("read status field")
            .expect("seeded status field")
    }

    /// The config the caller would send to drop `remove_ids` and keep everything
    /// else, exactly as a panel round-trips it: kept options carry their ids.
    fn config_without(field: &Field, remove_ids: &[&str]) -> FieldConfig {
        let mut config = field.config.clone();
        config
            .options
            .retain(|option| !remove_ids.contains(&option.id.as_str()));
        config
    }

    /// Building the router is what validates every path pattern. Two routes that
    /// conflict panic HERE, at `Router::new().route(...)`, not at `cargo check`.
    #[test]
    fn the_router_builds_with_every_route_registered() {
        let _routes = routes();
        let _alias = router();
    }

    // ── migrate_option_value: the pure core of the migration ───────────────────

    #[test]
    fn a_scalar_option_is_replaced_or_cleared() {
        let plan = remap(&[("opt_a", Some("opt_b")), ("opt_c", None)]);
        assert_eq!(
            migrate_option_value(Some(&json!("opt_a")), &plan),
            json!("opt_b")
        );
        assert_eq!(
            migrate_option_value(Some(&json!("opt_c")), &plan),
            Value::Null
        );
    }

    #[test]
    fn an_option_the_edit_does_not_touch_is_left_alone() {
        // The single most damaging bug this function could have: rewriting a value
        // whose option is still perfectly valid.
        let plan = remap(&[("opt_a", Some("opt_b"))]);
        assert_eq!(
            migrate_option_value(Some(&json!("opt_z")), &plan),
            json!("opt_z")
        );
        assert_eq!(
            migrate_option_value(Some(&json!(["opt_z", "opt_y"])), &plan),
            json!(["opt_z", "opt_y"])
        );
    }

    #[test]
    fn a_multi_select_keeps_its_survivors_and_dedupes_the_merge() {
        let plan = remap(&[("opt_a", Some("opt_b")), ("opt_c", None)]);
        // opt_a → opt_b, but opt_b is already there: one B, not two.
        assert_eq!(
            migrate_option_value(Some(&json!(["opt_a", "opt_b", "opt_z"])), &plan),
            json!(["opt_b", "opt_z"])
        );
        // A dropped-and-cleared element vanishes; the kept one survives.
        assert_eq!(
            migrate_option_value(Some(&json!(["opt_c", "opt_z"])), &plan),
            json!(["opt_z"])
        );
    }

    #[test]
    fn an_emptied_multi_select_clears_rather_than_storing_an_empty_array() {
        let plan = remap(&[("opt_a", None)]);
        assert_eq!(
            migrate_option_value(Some(&json!(["opt_a"])), &plan),
            Value::Null
        );
        assert_eq!(migrate_option_value(None, &plan), Value::Null);
        assert_eq!(migrate_option_value(Some(&json!(7)), &plan), Value::Null);
    }

    // ── Destructive-edit guards ────────────────────────────────────────────────

    #[tokio::test]
    async fn deleting_a_standard_object_is_a_conflict_not_a_500() {
        let state = state();
        // Addressed by SLUG, which is also what proves the path segment is resolved
        // before the guard runs — checking `is_standard` on an unresolved slug would
        // never fire.
        let error = delete_object(State(state.clone()), Path("company".into()))
            .await
            .expect_err("a standard object must not be deletable");
        assert!(matches!(error, ApiError::Conflict(_)), "{error}");
        assert!(
            state.store.get_object("company").await.unwrap().is_some(),
            "the object must still be there"
        );
    }

    #[tokio::test]
    async fn deleting_a_system_field_is_a_conflict() {
        let state = state();
        let error = delete_field(State(state.clone()), Path("fld_company_name".into()))
            .await
            .expect_err("a system field must not be deletable");
        assert!(matches!(error, ApiError::Conflict(_)), "{error}");
    }

    #[tokio::test]
    async fn deleting_the_objects_title_field_is_a_conflict() {
        let state = state();
        // Re-point the title at a deletable field, then try to delete it. Without the
        // guard this succeeds and every company silently loses its title on the next
        // write.
        patch_object(
            State(state.clone()),
            Path("company".into()),
            Json(UpdateObjectRequest {
                title_field_id: Some("fld_company_domain".into()),
                ..Default::default()
            }),
        )
        .await
        .expect("re-pointing the title at a real field is allowed");

        let error = delete_field(State(state.clone()), Path("fld_company_domain".into()))
            .await
            .expect_err("the title field must not be deletable");
        assert!(matches!(error, ApiError::Conflict(_)), "{error}");
    }

    #[tokio::test]
    async fn a_title_field_from_another_object_is_rejected() {
        let state = state();
        let error = patch_object(
            State(state.clone()),
            Path("company".into()),
            Json(UpdateObjectRequest {
                title_field_id: Some("fld_person_name".into()),
                ..Default::default()
            }),
        )
        .await
        .expect_err("a title field must belong to the object");
        assert!(matches!(error, ApiError::BadRequest(_)), "{error}");
    }

    #[tokio::test]
    async fn a_system_fields_flags_cannot_be_flipped_but_a_resend_is_fine() {
        let state = state();
        let name = state
            .store
            .get_field("fld_company_name")
            .await
            .unwrap()
            .unwrap();
        assert!(name.is_system && name.is_required);

        let error = patch_field(
            State(state.clone()),
            Path("fld_company_name".into()),
            Json(PatchFieldBody {
                field: UpdateFieldRequest {
                    is_required: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            }),
        )
        .await
        .expect_err("a system field's required flag is the product's, not the user's");
        assert!(matches!(error, ApiError::Conflict(_)), "{error}");

        // The same PATCH a panel sends when it round-trips the whole field unchanged
        // must NOT be rejected — guarding on presence instead of on change would make
        // every rename fail.
        let ok = patch_field(
            State(state.clone()),
            Path("fld_company_name".into()),
            Json(PatchFieldBody {
                field: UpdateFieldRequest {
                    name: Some("Company name".into()),
                    is_required: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            }),
        )
        .await
        .expect("an unchanged flag alongside a rename is not a flip")
        .0;
        assert_eq!(ok["name"], json!("Company name"));
        assert_eq!(ok["migrated_records"], json!(0));
    }

    #[tokio::test]
    async fn reorder_refuses_ids_from_another_object_and_duplicates() {
        let state = state();
        let stray = reorder_fields(
            State(state.clone()),
            Path("company".into()),
            Json(ReorderRequest {
                ids: vec!["fld_company_domain".into(), "fld_person_name".into()],
            }),
        )
        .await
        .expect_err("a field from another object would be repositioned into this one");
        assert!(matches!(stray, ApiError::BadRequest(_)), "{stray}");

        let duplicated = reorder_fields(
            State(state.clone()),
            Path("company".into()),
            Json(ReorderRequest {
                ids: vec!["fld_company_domain".into(), "fld_company_domain".into()],
            }),
        )
        .await
        .expect_err("a duplicate id silently wins twice");
        assert!(
            matches!(duplicated, ApiError::BadRequest(_)),
            "{duplicated}"
        );

        let ordered = reorder_fields(
            State(state.clone()),
            Path("company".into()),
            Json(ReorderRequest {
                ids: vec!["fld_company_domain".into()],
            }),
        )
        .await
        .expect("a well-formed reorder is applied")
        .0;
        assert_eq!(
            ordered.first().map(|f| f.id.as_str()),
            Some("fld_company_domain")
        );
    }

    #[tokio::test]
    async fn an_unknown_object_is_404_not_an_empty_field_list() {
        let state = state();
        let error = list_fields(State(state.clone()), Path("nope".into()))
            .await
            .expect_err("an unknown object is not an object with no fields");
        assert!(matches!(error, ApiError::NotFound(_)), "{error}");
    }

    // ── Option removal ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn dropping_an_unused_option_needs_no_permission() {
        let state = state();
        company(&state, "Acme", "opt_company_status_customer").await;
        let field = status_field(&state).await;

        // `lead` is defined but nothing is on it.
        let out = patch_field(
            State(state.clone()),
            Path(field.id.clone()),
            Json(PatchFieldBody {
                field: UpdateFieldRequest {
                    config: Some(config_without(&field, &["opt_company_status_lead"])),
                    ..Default::default()
                },
                ..Default::default()
            }),
        )
        .await
        .expect("an option nothing references is a pure schema edit")
        .0;
        assert_eq!(out["migrated_records"], json!(0));

        let after = status_field(&state).await;
        assert!(after.config.option("opt_company_status_lead").is_none());
        assert!(after.config.option("opt_company_status_customer").is_some());
    }

    #[tokio::test]
    async fn dropping_an_in_use_option_is_refused_with_the_option_named() {
        let state = state();
        company(&state, "Acme", "opt_company_status_lead").await;
        company(&state, "Globex", "opt_company_status_lead").await;
        let field = status_field(&state).await;

        let error = patch_field(
            State(state.clone()),
            Path(field.id.clone()),
            Json(PatchFieldBody {
                field: UpdateFieldRequest {
                    config: Some(config_without(&field, &["opt_company_status_lead"])),
                    ..Default::default()
                },
                ..Default::default()
            }),
        )
        .await
        .expect_err("dropping an in-use option must not silently orphan its records");
        assert!(matches!(error, ApiError::Conflict(_)), "{error}");
        let message = error.to_string();
        assert!(
            message.contains("Lead"),
            "the option must be named: {message}"
        );
        assert!(
            message.contains('2'),
            "the blast radius must be stated: {message}"
        );

        // And nothing moved: a refused request must not half-apply.
        let after = status_field(&state).await;
        assert!(after.config.option("opt_company_status_lead").is_some());
    }

    #[tokio::test]
    async fn a_soft_deleted_record_still_blocks_the_removal() {
        let state = state();
        let record = company(&state, "Acme", "opt_company_status_lead").await;
        assert!(state.store.delete_record(&record.id).await.unwrap());
        let field = status_field(&state).await;

        // Counting live rows only would report "safe" here, and restoring the record
        // afterwards would resurrect a value pointing at an option that is gone.
        let error = patch_field(
            State(state.clone()),
            Path(field.id.clone()),
            Json(PatchFieldBody {
                field: UpdateFieldRequest {
                    config: Some(config_without(&field, &["opt_company_status_lead"])),
                    ..Default::default()
                },
                ..Default::default()
            }),
        )
        .await
        .expect_err("a soft-deleted record is one restore away from being live");
        assert!(matches!(error, ApiError::Conflict(_)), "{error}");
    }

    #[tokio::test]
    async fn an_explicit_migration_rewrites_the_records_and_then_drops_the_option() {
        let state = state();
        let acme = company(&state, "Acme", "opt_company_status_lead").await;
        let globex = company(&state, "Globex", "opt_company_status_customer").await;
        let field = status_field(&state).await;

        let out = patch_field(
            State(state.clone()),
            Path(field.id.clone()),
            Json(PatchFieldBody {
                field: UpdateFieldRequest {
                    config: Some(config_without(&field, &["opt_company_status_lead"])),
                    ..Default::default()
                },
                option_migration: remap(&[(
                    "opt_company_status_lead",
                    Some("opt_company_status_prospect"),
                )]),
            }),
        )
        .await
        .expect("an explicit migration is permission to rewrite")
        .0;
        assert_eq!(out["migrated_records"], json!(1));

        let moved = state.store.get_record(&acme.id).await.unwrap().unwrap();
        assert_eq!(
            moved.values.get("status"),
            Some(&json!("opt_company_status_prospect"))
        );
        // The record that was never on the dropped option is untouched.
        let untouched = state.store.get_record(&globex.id).await.unwrap().unwrap();
        assert_eq!(
            untouched.values.get("status"),
            Some(&json!("opt_company_status_customer"))
        );
        assert!(status_field(&state)
            .await
            .config
            .option("opt_company_status_lead")
            .is_none());
    }

    #[tokio::test]
    async fn a_null_migration_clears_the_cells() {
        let state = state();
        let acme = company(&state, "Acme", "opt_company_status_lead").await;
        let field = status_field(&state).await;

        let out = patch_field(
            State(state.clone()),
            Path(field.id.clone()),
            Json(PatchFieldBody {
                field: UpdateFieldRequest {
                    config: Some(config_without(&field, &["opt_company_status_lead"])),
                    ..Default::default()
                },
                option_migration: remap(&[("opt_company_status_lead", None)]),
            }),
        )
        .await
        .expect("clearing is a valid migration")
        .0;
        assert_eq!(out["migrated_records"], json!(1));

        let cleared = state.store.get_record(&acme.id).await.unwrap().unwrap();
        assert!(
            cleared.values.get("status").is_none(),
            "a cleared cell holds nothing, not a stale id: {:?}",
            cleared.values
        );
    }

    #[tokio::test]
    async fn a_migration_onto_an_option_this_request_also_removes_is_rejected() {
        let state = state();
        company(&state, "Acme", "opt_company_status_lead").await;
        let field = status_field(&state).await;

        let error = patch_field(
            State(state.clone()),
            Path(field.id.clone()),
            Json(PatchFieldBody {
                field: UpdateFieldRequest {
                    config: Some(config_without(
                        &field,
                        &["opt_company_status_lead", "opt_company_status_prospect"],
                    )),
                    ..Default::default()
                },
                // Migrating onto something that is also going away is a migration to
                // nowhere — and the rewrite loop would never make progress.
                option_migration: remap(&[(
                    "opt_company_status_lead",
                    Some("opt_company_status_prospect"),
                )]),
            }),
        )
        .await
        .expect_err("a migration target must survive the edit");
        assert!(matches!(error, ApiError::BadRequest(_)), "{error}");
    }

    #[tokio::test]
    async fn a_migration_naming_an_option_that_is_not_being_removed_is_rejected() {
        let state = state();
        let field = status_field(&state).await;

        let error = patch_field(
            State(state.clone()),
            Path(field.id.clone()),
            Json(PatchFieldBody {
                field: UpdateFieldRequest {
                    name: Some("Lifecycle".into()),
                    ..Default::default()
                },
                option_migration: remap(&[(
                    "opt_company_status_lead",
                    Some("opt_company_status_prospect"),
                )]),
            }),
        )
        .await
        .expect_err("a migration for a removal that is not happening is a disagreement");
        assert!(matches!(error, ApiError::BadRequest(_)), "{error}");
    }

    #[tokio::test]
    async fn a_migration_onto_a_brand_new_option_resolves_after_the_store_assigns_its_id() {
        let state = state();
        let acme = company(&state, "Acme", "opt_company_status_lead").await;
        let field = status_field(&state).await;

        // The replacement has no id yet — the store derives one on write. This is the
        // case that CANNOT be rewritten before the config lands, so it exercises the
        // config-first branch and the resolve-by-label leg.
        let mut config = config_without(&field, &["opt_company_status_lead"]);
        config.options.push(SelectOption::new(
            "",
            "Evaluating",
            config.options.len() as i64,
        ));

        let out = patch_field(
            State(state.clone()),
            Path(field.id.clone()),
            Json(PatchFieldBody {
                field: UpdateFieldRequest {
                    config: Some(config),
                    ..Default::default()
                },
                option_migration: remap(&[("opt_company_status_lead", Some("Evaluating"))]),
            }),
        )
        .await
        .expect("a newly added option is a valid migration target")
        .0;
        assert_eq!(out["migrated_records"], json!(1));

        let after = status_field(&state).await;
        let evaluating = after
            .config
            .options
            .iter()
            .find(|option| option.label == "Evaluating")
            .expect("the new option was written");
        assert!(
            !evaluating.id.is_empty(),
            "the store assigns the id, and the records must store THAT"
        );
        let moved = state.store.get_record(&acme.id).await.unwrap().unwrap();
        assert_eq!(moved.values.get("status"), Some(&json!(evaluating.id)));
    }

    #[tokio::test]
    async fn a_multi_select_migration_rewrites_every_element() {
        let state = state();
        // `tags` is seeded as a multi_select with no options; give it three, then drop
        // one. This is the array path end to end, not just the pure function.
        let tags = state
            .store
            .get_field("fld_company_tags")
            .await
            .unwrap()
            .unwrap();
        let mut config = tags.config.clone();
        config.options = vec![
            SelectOption::new("opt_tags_smb", "SMB", 0),
            SelectOption::new("opt_tags_mid", "Mid-market", 1),
            SelectOption::new("opt_tags_ent", "Enterprise", 2),
        ];
        patch_field(
            State(state.clone()),
            Path(tags.id.clone()),
            Json(PatchFieldBody {
                field: UpdateFieldRequest {
                    config: Some(config.clone()),
                    ..Default::default()
                },
                ..Default::default()
            }),
        )
        .await
        .expect("adding options to an empty multi_select drops nothing");

        let record = state
            .store
            .create_record(
                "obj_company",
                &CreateRecordRequest {
                    values: bag(&[
                        ("name", json!("Acme")),
                        ("tags", json!(["opt_tags_smb", "opt_tags_ent"])),
                    ]),
                    created_by: None,
                },
            )
            .await
            .unwrap()
            .unwrap();

        config.options.retain(|option| option.id != "opt_tags_smb");
        let out = patch_field(
            State(state.clone()),
            Path(tags.id.clone()),
            Json(PatchFieldBody {
                field: UpdateFieldRequest {
                    config: Some(config),
                    ..Default::default()
                },
                option_migration: remap(&[("opt_tags_smb", Some("opt_tags_mid"))]),
            }),
        )
        .await
        .expect("a multi_select migration is the same permission")
        .0;
        assert_eq!(out["migrated_records"], json!(1));

        let moved = state.store.get_record(&record.id).await.unwrap().unwrap();
        assert_eq!(
            moved.values.get("tags"),
            Some(&json!(["opt_tags_mid", "opt_tags_ent"])),
            "the untouched element must survive in place"
        );
    }
}
