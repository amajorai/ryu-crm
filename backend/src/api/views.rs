//! Saved views and lists — the two ways Harbor narrows an object down to the rows a
//! person actually wants to look at.
//!
//! They are NOT the same primitive, and the split is the whole reason both exist:
//!
//! - A **view** is a *query*. It holds a filter tree, a sort list, which columns to
//!   render and (for a board) which option-backed field to group by. Everything it
//!   shows is derived from the records; nothing is stored per row. Change a deal's
//!   stage and it moves column on its own.
//! - A **list** is a *set*. A human put each record in it, and the membership itself
//!   carries data — a deal's stage *inside this particular sales cycle*, which is a
//!   different fact from the deal's own `stage` field and must never overwrite it.
//!   That per-membership data lives in [`ListEntry::values`], keyed by the slugs of
//!   fields whose `list_id` is set: a **separate namespace** from the record's own
//!   bag. A list may have its own `stage` and the object may have `stage` too, and
//!   the two never collide because they are looked up in different field tables.
//!
//! Every path here is MOUNT-LOCAL — `main` nests the module router under
//! `/api/crm`, and Core's ext-proxy rewrites the external
//! `/api/ext/@ryu/crm/*` onto the same prefix. Writing `/api/crm/views` in this
//! file would serve `/api/crm/api/crm/views`.
//!
//! Response conventions, matching the rest of the sidecar:
//!
//! - **Every response is JSON**, including deletes and reorders, which return
//!   `{"ok": true}`. A bodiless 204 is indistinguishable from a dropped connection
//!   on the panel side.
//! - **Plain collection reads are wrapped by their plural** (`{"views": […]}`),
//!   never a bare array, so a response stays extensible.
//! - **Contract-specified envelopes are returned bare**: [`Page`] (as
//!   [`ListEntryPage`]) and [`ViewResult`] are the wire shapes the panel binds to,
//!   and wrapping them would add a level the contract does not describe.

use axum::{
    extract::{Path, Query, State},
    routing::{get, patch, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::models::*;
use crate::state::AppState;

/// Build the views + lists half of the HTTP surface.
///
/// Returns an un-stated `Router<AppState>`: `main` merges the six router modules and
/// calls `.with_state(state)` exactly once, so no module may consume the state itself.
///
/// Route order is for readability only — axum gives a static segment priority over a
/// parameter regardless of insertion order, so `/lists/:list_id/entries/query` can
/// never be captured as an entry id.
pub fn routes() -> Router<AppState> {
    Router::new()
        // ── Views ──
        .route("/objects/:object/views", get(list_views).post(create_view))
        .route(
            "/views/:view_id",
            get(get_view).patch(patch_view).delete(delete_view),
        )
        .route("/views/:view_id/default", post(set_default_view))
        .route("/views/:view_id/run", post(run_view))
        // ── Lists ──
        .route("/lists", get(list_lists).post(create_list))
        .route(
            "/lists/:list_id",
            get(get_list).patch(patch_list).delete(delete_list),
        )
        // A list's OWN fields — the extra columns that exist only inside this list.
        .route(
            "/lists/:list_id/fields",
            get(list_list_fields).post(create_list_field),
        )
        // ── List entries ──
        .route("/lists/:list_id/entries", post(add_list_entry))
        .route("/lists/:list_id/entries/query", post(query_list_entries))
        .route("/lists/:list_id/entries/reorder", post(reorder_list_entries))
        // Entries are addressed at the top level rather than under their list: an
        // entry id already identifies its list, and a nested path would let a caller
        // pass a list it does not belong to and expect that to mean something.
        .route(
            "/list-entries/:entry_id",
            patch(patch_list_entry).delete(remove_list_entry),
        )
}

/// The name the sibling router modules were briefed to export.
///
/// An alias rather than a rename: the foundation contract specifies `routes()` and
/// `main` is owned by another agent, so exporting both means whichever name that file
/// reaches for resolves. `#[allow(dead_code)]` because exactly one of the two will be
/// called and the other must not warn.
#[allow(dead_code)]
pub fn router() -> Router<AppState> {
    routes()
}

/// Turn a store `bool` "did anything change" into a 404, so a caller can tell a
/// missing row from a successful no-op.
fn require_hit(changed: bool, what: &str) -> ApiResult<()> {
    if changed {
        Ok(())
    } else {
        Err(ApiError::not_found(what))
    }
}

// ── Views ──────────────────────────────────────────────────────────────────────

/// `GET /objects/{object}/views` — every saved view on one object, in position order.
///
/// The object is resolved FIRST even though `list_views` already tolerates an unknown
/// id by returning an empty vector. Every object has at least one view (creating one
/// creates its default table view, and `delete_view` refuses the last), so an empty
/// array here would always mean "no such object" — reporting that as a successful
/// empty list would render as an object with no views, which cannot happen.
async fn list_views(
    State(state): State<AppState>,
    Path(object): Path<String>,
) -> ApiResult<Json<Value>> {
    let object = state
        .store
        .get_object(&object)
        .await?
        .ok_or_else(|| ApiError::not_found("object"))?;
    let views = state.store.list_views(&object.id).await?;
    Ok(Json(json!({ "views": views })))
}

/// `POST /objects/{object}/views` — save a new view.
///
/// The object is resolved before the write because `create_view` `bail!`s on an
/// unknown object, and a `bail!` becomes `ApiError::Internal` → 500. A path segment
/// that names nothing is a 404, not a server fault.
async fn create_view(
    State(state): State<AppState>,
    Path(object): Path<String>,
    Json(body): Json<CreateViewRequest>,
) -> ApiResult<Json<View>> {
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("a view needs a name"));
    }
    let object = state
        .store
        .get_object(&object)
        .await?
        .ok_or_else(|| ApiError::not_found("object"))?;
    Ok(Json(state.store.create_view(&object.id, &body).await?))
}

async fn get_view(
    State(state): State<AppState>,
    Path(view_id): Path<String>,
) -> ApiResult<Json<View>> {
    state
        .store
        .get_view(&view_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("view"))
}

/// `PATCH /views/{view_id}` — partial update.
///
/// Absent means unchanged for every field. One documented gap, inherited from the
/// store's `.or(existing)` merge: `group_by_field_id` cannot be CLEARED through this
/// route, only re-pointed. Deleting the grouping field clears it, which is the path
/// that actually occurs; a board mid-configuration degrades to a single column rather
/// than erroring, so a stale grouping is cosmetic rather than fatal.
async fn patch_view(
    State(state): State<AppState>,
    Path(view_id): Path<String>,
    Json(body): Json<UpdateViewRequest>,
) -> ApiResult<Json<View>> {
    state
        .store
        .update_view(&view_id, &body)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("view"))
}

/// `DELETE /views/{view_id}`.
///
/// The last view on an object is refused with 409, checked HERE rather than by
/// mapping every store error to a conflict — the store signals it with `bail!`, which
/// would otherwise surface as a 500 and send whoever debugs it looking for a SQL
/// fault. Deleting the default promotes the next view in position order, so an object
/// is never left with views but no default.
async fn delete_view(
    State(state): State<AppState>,
    Path(view_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let view = state
        .store
        .get_view(&view_id)
        .await?
        .ok_or_else(|| ApiError::not_found("view"))?;
    if state.store.list_views(&view.object_id).await?.len() <= 1 {
        return Err(ApiError::conflict(
            "an object must keep at least one view — rename this one instead of deleting it",
        ));
    }
    require_hit(state.store.delete_view(&view.id).await?, "view")?;
    Ok(Json(json!({ "ok": true })))
}

/// `POST /views/{view_id}/default` — make this the view opening the object shows.
///
/// Exactly one default per object: the store demotes the previous one in the same
/// transaction. The updated view is returned rather than `{"ok": true}` so the sidebar
/// can re-render from the response instead of refetching.
async fn set_default_view(
    State(state): State<AppState>,
    Path(view_id): Path<String>,
) -> ApiResult<Json<View>> {
    require_hit(state.store.set_default_view(&view_id).await?, "view")?;
    state
        .store
        .get_view(&view_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("view"))
}

/// `POST /views/{view_id}/run` — everything the table or board needs to paint.
///
/// The body is OPTIONAL: opening a saved view is one click and sends no payload, and
/// requiring `{}` would make the commonest call the fiddliest. When present, its
/// `filter` is ANDed with the view's saved filter (never replacing it — a view named
/// "Open deals" that a search box could silently widen is a lie), while `sorts`
/// replaces, because clicking a column header means "sort by this instead".
///
/// `limit`/`offset` are clamped HERE. The store takes them as positional arguments
/// and uses them verbatim; the fields on `ViewQueryOverrides` are wire-only and the
/// store ignores them, so forwarding `overrides.limit` would compile, look right, and
/// have no ceiling at all.
async fn run_view(
    State(state): State<AppState>,
    Path(view_id): Path<String>,
    body: Option<Json<ViewQueryOverrides>>,
) -> ApiResult<Json<ViewResult>> {
    let overrides = body.map(|Json(b)| b).unwrap_or_default();
    let limit = state.config.clamp_limit(overrides.limit);
    let offset = overrides.offset.unwrap_or(0);
    state
        .store
        .run_view(&view_id, &overrides, limit, offset)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("view"))
}

// ── Lists ──────────────────────────────────────────────────────────────────────

/// `GET /lists?object_id=` — every list, or one object's.
#[derive(Debug, Default, Deserialize)]
struct ListScopeQuery {
    #[serde(default)]
    object_id: Option<String>,
}

async fn list_lists(
    State(state): State<AppState>,
    Query(scope): Query<ListScopeQuery>,
) -> ApiResult<Json<Value>> {
    let object_id = match scope
        .object_id
        .as_deref()
        .map(str::trim)
        .filter(|o| !o.is_empty())
    {
        // The filter value is resolved rather than passed through: `list_lists`
        // answers an unknown object with an empty vector, which renders as "this
        // object has no lists" and hides the typo that caused it.
        Some(reference) => Some(
            state
                .store
                .get_object(reference)
                .await?
                .ok_or_else(|| ApiError::bad_request(format!("unknown object \"{reference}\"")))?
                .id,
        ),
        None => None,
    };
    let lists = state.store.list_lists(object_id.as_deref()).await?;
    Ok(Json(json!({ "lists": lists })))
}

/// `POST /lists` — a curated subset of one object's records.
///
/// `object_id` arrives in the BODY here (a list is not addressed under its object,
/// because it is addressable on its own afterwards), so an unknown one is a 400 —
/// the caller's payload referenced something that does not exist — rather than the
/// 404 a bad path segment earns.
async fn create_list(
    State(state): State<AppState>,
    Json(body): Json<CreateListRequest>,
) -> ApiResult<Json<List>> {
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("a list needs a name"));
    }
    let object = state
        .store
        .get_object(&body.object_id)
        .await?
        .ok_or_else(|| ApiError::bad_request(format!("unknown object \"{}\"", body.object_id)))?;
    // Resolved id, not the caller's string: `object_id` accepts a slug and every
    // stored reference must be the canonical id.
    let request = CreateListRequest {
        object_id: object.id,
        ..body
    };
    Ok(Json(state.store.create_list(&request).await?))
}

async fn get_list(
    State(state): State<AppState>,
    Path(list_id): Path<String>,
) -> ApiResult<Json<List>> {
    state
        .store
        .get_list(&list_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("list"))
}

async fn patch_list(
    State(state): State<AppState>,
    Path(list_id): Path<String>,
    Json(body): Json<UpdateListRequest>,
) -> ApiResult<Json<List>> {
    state
        .store
        .update_list(&list_id, &body)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("list"))
}

/// `DELETE /lists/{list_id}` — drops the membership, its list-specific fields and
/// their values.
///
/// **The records survive.** A list is a set; removing the set must not remove its
/// members. What is lost is the per-membership data, which is the point of the
/// warning the panel shows before calling this.
async fn delete_list(
    State(state): State<AppState>,
    Path(list_id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_hit(state.store.delete_list(&list_id).await?, "list")?;
    Ok(Json(json!({ "ok": true })))
}

/// `GET /lists/{list_id}/fields` — the list's OWN extra columns.
///
/// Never the object's fields: this is the separate namespace described in the module
/// docs, and a panel that rendered both from one call could not tell which bag to
/// write an edit into.
async fn list_list_fields(
    State(state): State<AppState>,
    Path(list_id): Path<String>,
) -> ApiResult<Json<Value>> {
    // Resolved first so an unknown list is a 404 rather than an empty column set,
    // which would look like a list that simply has no extra fields.
    let list = state
        .store
        .get_list(&list_id)
        .await?
        .ok_or_else(|| ApiError::not_found("list"))?;
    let fields = state.store.list_list_fields(&list.id).await?;
    Ok(Json(json!({ "fields": fields })))
}

/// `POST /lists/{list_id}/fields` — add an extra column to this list.
///
/// `is_unique` is deliberately NOT pre-checked here even though it is always invalid
/// on a list field: the store rejects it as a field-level validation error, which
/// reaches the panel as a 422 with the offending field named, and duplicating the
/// check would give the same mistake two different error shapes depending on which
/// guard fired first.
async fn create_list_field(
    State(state): State<AppState>,
    Path(list_id): Path<String>,
    Json(body): Json<CreateFieldRequest>,
) -> ApiResult<Json<Field>> {
    let list = state
        .store
        .get_list(&list_id)
        .await?
        .ok_or_else(|| ApiError::not_found("list"))?;
    // The field's `object_id` is the LIST's object — a list field is still "about"
    // that object, it just stores its values on the membership row.
    match state
        .store
        .create_field(&list.object_id, Some(&list.id), &body)
        .await?
    {
        Ok(field) => Ok(Json(field)),
        Err(errors) => Err(ApiError::validation(errors)),
    }
}

// ── List entries ───────────────────────────────────────────────────────────────

/// `POST /lists/{list_id}/entries` — put a record in the list.
///
/// Re-adding an existing member is a successful no-op that returns the membership
/// that already existed, not a 409: "add to list" is an idempotent gesture and a
/// double-click must not be an error.
async fn add_list_entry(
    State(state): State<AppState>,
    Path(list_id): Path<String>,
    Json(body): Json<AddListEntryRequest>,
) -> ApiResult<Json<ListEntry>> {
    // `add_list_entry` `bail!`s on an unknown list (→ 500); resolve first so a stale
    // list id in the panel is the 404 it actually is. A record from another object,
    // by contrast, is a *validation* failure the store reports per-field.
    let list = state
        .store
        .get_list(&list_id)
        .await?
        .ok_or_else(|| ApiError::not_found("list"))?;
    match state.store.add_list_entry(&list.id, &body).await? {
        Ok(entry) => Ok(Json(entry)),
        Err(errors) => Err(ApiError::validation(errors)),
    }
}

/// `POST /lists/{list_id}/entries/query` — the list table's page.
///
/// Filters and sorts may name EITHER the list's own fields or the record's; the store
/// resolves each key against the list fields first and binds it to the entry row,
/// falling back to the record. That is what lets a list be sorted by its private
/// `stage` and filtered by the company's `industry` in one query.
///
/// The body is optional (opening a list is a click, not a form) and `list_id` is
/// taken from the PATH, overwriting whatever the body carried: `ListEntryQuery` has
/// its own `list_id` field, and honouring it would let `POST /lists/A/entries/query`
/// with `{"list_id":"B"}` return list B's rows under list A's URL.
///
/// `limit`/`offset` are clamped here for the same reason as `run_view` — the store
/// ignores the struct's copies and uses the positional arguments verbatim.
async fn query_list_entries(
    State(state): State<AppState>,
    Path(list_id): Path<String>,
    body: Option<Json<ListEntryQuery>>,
) -> ApiResult<Json<ListEntryPage>> {
    let list = state
        .store
        .get_list(&list_id)
        .await?
        .ok_or_else(|| ApiError::not_found("list"))?;
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let limit = state.config.clamp_limit(body.limit);
    let offset = body.offset.unwrap_or(0);
    let query = ListEntryQuery {
        list_id: list.id,
        ..body
    };
    Ok(Json(
        state.store.query_list_entries(&query, limit, offset).await?,
    ))
}

/// `POST /lists/{list_id}/entries/reorder` — manual ordering within the list.
///
/// The listed entry ids get positions `0..n`; anything omitted keeps its relative
/// order after them, so a drag inside a loaded page does not disturb the rows below
/// it. `reorder_list_entries` scopes every UPDATE to the list, so an id from another
/// list is ignored rather than stolen — but it also returns `Ok(())` unconditionally,
/// which is why the list is resolved first: without that, a reorder against a deleted
/// list would answer 200.
async fn reorder_list_entries(
    State(state): State<AppState>,
    Path(list_id): Path<String>,
    Json(body): Json<ReorderRequest>,
) -> ApiResult<Json<Value>> {
    let list = state
        .store
        .get_list(&list_id)
        .await?
        .ok_or_else(|| ApiError::not_found("list"))?;
    state.store.reorder_list_entries(&list.id, &body.ids).await?;
    Ok(Json(json!({ "ok": true })))
}

/// `PATCH /list-entries/{entry_id}` — edit the LIST-SPECIFIC values on one membership.
///
/// **This is not a record edit.** The bag is keyed by the slugs of fields whose
/// `list_id` is this list, and it never touches [`Record::values`] — a list's own
/// `stage` and the deal's `fld_deal_stage` are different facts about different
/// things, and writing one through the other is the confusion this whole namespace
/// split exists to prevent. Editing the record itself is `PATCH /records/{id}`.
///
/// `mode` follows the record convention: `merge` (the default, what a cell edit
/// sends) touches only the named slugs and clears one mapped to `null`; `replace`
/// makes the bag exactly what was sent.
async fn patch_list_entry(
    State(state): State<AppState>,
    Path(entry_id): Path<String>,
    Json(body): Json<UpdateListEntryRequest>,
) -> ApiResult<Json<ListEntry>> {
    match state.store.update_list_entry(&entry_id, &body).await? {
        Ok(Some(entry)) => Ok(Json(entry)),
        Ok(None) => Err(ApiError::not_found("list entry")),
        Err(errors) => Err(ApiError::validation(errors)),
    }
}

/// `DELETE /list-entries/{entry_id}` — remove the membership only.
///
/// The record and its own field values are untouched; so is every other list it
/// belongs to. Only this membership's list-specific values are lost.
async fn remove_list_entry(
    State(state): State<AppState>,
    Path(entry_id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_hit(state.store.remove_list_entry(&entry_id).await?, "list entry")?;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MAX_PAGE_SIZE;

    fn state() -> AppState {
        AppState::in_memory().expect("in-memory state")
    }

    /// Building the router is what validates every path pattern. Two routes that
    /// conflict — or a parameter renamed out of step with a sibling module's
    /// `/objects/:object/...` — panic HERE, at `Router::new().route(...)`, not at
    /// `cargo check`.
    #[test]
    fn the_router_builds_with_every_route_registered() {
        let _routes = routes();
        let _alias = router();
    }

    /// A seeded deal, ready to be dropped into a list.
    async fn seed_deal(state: &AppState, name: &str, stage: &str) -> Record {
        let mut values = ValueBag::new();
        values.insert("name".into(), json!(name));
        values.insert("stage".into(), json!(stage));
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
            .expect("create_record")
            .expect("the seeded deal is valid")
    }

    // ── Views ──────────────────────────────────────────────────────────────────

    /// An object with no view cannot be opened at all, so the store refuses to delete
    /// the last one — with a `bail!`, which becomes a 500 unless the handler
    /// pre-checks. 409 is the contract; 500 would read as "Harbor is broken".
    #[tokio::test]
    async fn deleting_the_last_view_is_a_conflict_not_a_server_error() {
        let state = state();
        // `note` is seeded with exactly one view, so it is already at the floor.
        let views = state.store.list_views(OBJ_NOTE).await.unwrap();
        assert_eq!(views.len(), 1, "the note object seeds one view");

        let err = delete_view(State(state.clone()), Path(VIEW_NOTE_ALL.to_string()))
            .await
            .expect_err("the last view must not be deletable");
        assert!(
            matches!(err, ApiError::Conflict(_)),
            "expected a 409, got {err:?}"
        );

        // And it really is still there — the guard must not have half-deleted it.
        assert!(state.store.get_view(VIEW_NOTE_ALL).await.unwrap().is_some());
    }

    /// Deleting the DEFAULT view must promote another, or the object opens onto
    /// nothing. The deal object seeds two views, so this is the one path that can
    /// legitimately remove a default.
    #[tokio::test]
    async fn deleting_the_default_view_promotes_another() {
        let state = state();
        let before = state.store.list_views(OBJ_DEAL).await.unwrap();
        assert!(before.iter().any(|v| v.id == VIEW_DEAL_ALL && v.is_default));

        let _ = delete_view(State(state.clone()), Path(VIEW_DEAL_ALL.to_string()))
            .await
            .expect("a non-last view is deletable");

        let after = state.store.list_views(OBJ_DEAL).await.unwrap();
        assert_eq!(after.iter().filter(|v| v.is_default).count(), 1);
        assert!(!after.iter().any(|v| v.id == VIEW_DEAL_ALL));
    }

    /// Exactly one default per object, on every path that can set one.
    #[tokio::test]
    async fn setting_a_default_demotes_the_previous_one() {
        let state = state();
        let view = set_default_view(State(state.clone()), Path(VIEW_DEAL_PIPELINE.to_string()))
            .await
            .expect("the pipeline board exists")
            .0;
        assert!(view.is_default, "the response must reflect the new default");

        let views = state.store.list_views(OBJ_DEAL).await.unwrap();
        assert_eq!(views.iter().filter(|v| v.is_default).count(), 1);
        assert!(views
            .iter()
            .any(|v| v.id == VIEW_DEAL_PIPELINE && v.is_default));
    }

    /// A view id that does not exist is a 404 from `run`, not an empty page — an
    /// empty page renders as "this view has no records", which is a different and
    /// much more confusing answer.
    #[tokio::test]
    async fn running_an_unknown_view_is_a_404() {
        let state = state();
        let err = run_view(State(state), Path("view_nope".to_string()), None)
            .await
            .expect_err("unknown view");
        assert!(matches!(err, ApiError::NotFound(_)), "got {err:?}");
    }

    /// The clamp that has no compiler to enforce it.
    ///
    /// `run_view` takes `limit` as a positional argument and uses it verbatim; the
    /// `limit` field on `ViewQueryOverrides` is wire-only and the store ignores it.
    /// A handler that forwarded `overrides.limit` would compile, pass any test that
    /// only checks the rows, and let one request ask for every record in the
    /// database — each carrying an unbounded JSON bag.
    #[tokio::test]
    async fn running_a_view_clamps_a_hostile_limit() {
        let state = state();
        let result = run_view(
            State(state),
            Path(VIEW_DEAL_ALL.to_string()),
            Some(Json(ViewQueryOverrides {
                limit: Some(100_000),
                ..Default::default()
            })),
        )
        .await
        .expect("the seeded view runs")
        .0;
        assert_eq!(result.page.limit, MAX_PAGE_SIZE);
    }

    /// No body at all is the common case (opening a view is a click), and it must
    /// yield the configured default page size rather than a 400 for a missing body.
    #[tokio::test]
    async fn running_a_view_without_a_body_uses_the_default_page_size() {
        let state = state();
        let expected = state.config.default_page_size;
        let result = run_view(State(state), Path(VIEW_DEAL_ALL.to_string()), None)
            .await
            .expect("a bodiless run is valid")
            .0;
        assert_eq!(result.page.limit, expected);
        assert_eq!(result.page.offset, 0);
    }

    // ── Lists ──────────────────────────────────────────────────────────────────

    /// **The namespace test.** A list's own `stage` and the deal's `fld_deal_stage`
    /// share a slug and mean different things: "where is this deal in THIS sales
    /// cycle" versus "where is this deal". Writing the list-specific one through
    /// `PATCH /list-entries/{id}` must leave the record's own stage exactly as it
    /// was — the confusion this whole split exists to prevent, and the one that would
    /// silently corrupt the pipeline report if it regressed.
    #[tokio::test]
    async fn a_lists_own_stage_never_touches_the_records_own_stage() {
        let state = state();
        let deal = seed_deal(&state, "Acme renewal", OPT_DEAL_STAGE_QUALIFIED).await;

        let list = create_list(
            State(state.clone()),
            Json(CreateListRequest {
                object_id: "deal".into(), // by SLUG, to prove the resolve happens
                name: "Q3 enterprise".into(),
                ..Default::default()
            }),
        )
        .await
        .expect("create the list")
        .0;
        assert_eq!(list.object_id, OBJ_DEAL, "the slug must resolve to the id");

        // The list's private stage — same slug as the deal's, different options.
        let list_field = create_list_field(
            State(state.clone()),
            Path(list.id.clone()),
            Json(CreateFieldRequest {
                slug: "stage".into(),
                name: "Cycle stage".into(),
                field_type: FieldType::Status,
                config: FieldConfig {
                    options: vec![
                        SelectOption::new("", "Pitched", 0),
                        SelectOption::new("", "Piloting", 1),
                    ],
                    ..Default::default()
                },
                ..Default::default()
            }),
        )
        .await
        .expect("a list may have its own stage")
        .0;
        assert_eq!(list_field.list_id.as_deref(), Some(list.id.as_str()));
        assert_eq!(
            list_field.object_id, OBJ_DEAL,
            "a list field is still about the list's object"
        );
        let piloting = list_field
            .config
            .options
            .iter()
            .find(|o| o.label == "Piloting")
            .expect("the option survived creation")
            .id
            .clone();

        let entry = add_list_entry(
            State(state.clone()),
            Path(list.id.clone()),
            Json(AddListEntryRequest {
                record_id: deal.id.clone(),
                ..Default::default()
            }),
        )
        .await
        .expect("the deal joins the list")
        .0;

        let mut values = ValueBag::new();
        values.insert("stage".into(), json!(piloting));
        let patched = patch_list_entry(
            State(state.clone()),
            Path(entry.id.clone()),
            Json(UpdateListEntryRequest {
                values,
                mode: UpdateMode::Merge,
            }),
        )
        .await
        .expect("the list-specific stage is writable")
        .0;

        assert_eq!(
            patched.values.get("stage"),
            Some(&json!(piloting)),
            "the membership carries the list's own stage"
        );

        let record = state
            .store
            .get_record(&deal.id)
            .await
            .unwrap()
            .expect("the deal still exists");
        assert_eq!(
            record.values.get("stage"),
            Some(&json!(OPT_DEAL_STAGE_QUALIFIED)),
            "the DEAL's own stage must be untouched by a list-entry edit"
        );
    }

    /// Re-adding a record already in the list is a no-op returning the SAME
    /// membership, not a second entry and not a 409. "Add to list" is an idempotent
    /// gesture; a double-click must not duplicate a row whose list-specific values
    /// would then diverge.
    #[tokio::test]
    async fn adding_a_record_twice_returns_the_same_membership() {
        let state = state();
        let deal = seed_deal(&state, "Repeat", OPT_DEAL_STAGE_LEAD).await;
        let list = state
            .store
            .create_list(&CreateListRequest {
                object_id: OBJ_DEAL.into(),
                name: "Watchlist".into(),
                ..Default::default()
            })
            .await
            .unwrap();

        let add = || {
            add_list_entry(
                State(state.clone()),
                Path(list.id.clone()),
                Json(AddListEntryRequest {
                    record_id: deal.id.clone(),
                    ..Default::default()
                }),
            )
        };
        let first = add().await.expect("first add").0;
        let second = add().await.expect("second add is a no-op, not an error").0;
        assert_eq!(first.id, second.id);
    }

    /// A record from another object cannot join the list — its list-specific fields
    /// would be untypeable. That is a *validation* failure (422 with the offending
    /// key named), not a 400, so the panel can point at the picker.
    #[tokio::test]
    async fn a_record_from_another_object_cannot_join_the_list() {
        let state = state();
        let mut values = ValueBag::new();
        values.insert("name".into(), json!("Acme Inc"));
        let company = state
            .store
            .create_record(
                OBJ_COMPANY,
                &CreateRecordRequest {
                    values,
                    created_by: None,
                },
            )
            .await
            .unwrap()
            .unwrap();
        let list = state
            .store
            .create_list(&CreateListRequest {
                object_id: OBJ_DEAL.into(),
                name: "Deals only".into(),
                ..Default::default()
            })
            .await
            .unwrap();

        let err = add_list_entry(
            State(state.clone()),
            Path(list.id),
            Json(AddListEntryRequest {
                record_id: company.id,
                ..Default::default()
            }),
        )
        .await
        .expect_err("a company is not a deal");
        assert!(matches!(err, ApiError::Validation(_)), "got {err:?}");
    }

    /// `is_unique` on a list field would enforce NOTHING — uniqueness is checked
    /// against `records`, and a list field's values live in `list_entries.data`. The
    /// store rejects it; this asserts the handler surfaces that as a 422 rather than
    /// swallowing it or turning it into a 500.
    #[tokio::test]
    async fn a_unique_list_field_is_rejected_as_a_field_error() {
        let state = state();
        let list = state
            .store
            .create_list(&CreateListRequest {
                object_id: OBJ_DEAL.into(),
                name: "Cycle".into(),
                ..Default::default()
            })
            .await
            .unwrap();

        let err = create_list_field(
            State(state.clone()),
            Path(list.id),
            Json(CreateFieldRequest {
                slug: "ticket".into(),
                name: "Ticket".into(),
                field_type: FieldType::Text,
                is_unique: true,
                ..Default::default()
            }),
        )
        .await
        .expect_err("a unique list field is not enforceable");
        assert!(matches!(err, ApiError::Validation(_)), "got {err:?}");
    }

    /// The path's list id wins over the body's.
    ///
    /// `ListEntryQuery` carries its own `list_id`, so a body naming a different list
    /// under `POST /lists/{A}/entries/query` would serve list B's rows at list A's
    /// URL — a cross-list read through a URL that looks scoped.
    #[tokio::test]
    async fn the_path_list_id_overrides_the_body() {
        let state = state();
        let deal = seed_deal(&state, "In A", OPT_DEAL_STAGE_LEAD).await;
        let a = state
            .store
            .create_list(&CreateListRequest {
                object_id: OBJ_DEAL.into(),
                name: "A".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let b = state
            .store
            .create_list(&CreateListRequest {
                object_id: OBJ_DEAL.into(),
                name: "B".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        state
            .store
            .add_list_entry(
                &a.id,
                &AddListEntryRequest {
                    record_id: deal.id.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();

        // Ask list B, but claim list A in the body.
        let page = query_list_entries(
            State(state.clone()),
            Path(b.id.clone()),
            Some(Json(ListEntryQuery {
                list_id: a.id.clone(),
                ..Default::default()
            })),
        )
        .await
        .expect("the query runs")
        .0;
        assert_eq!(page.total, 0, "the body must not redirect the read to list A");

        let page = query_list_entries(
            State(state.clone()),
            Path(a.id.clone()),
            Some(Json(ListEntryQuery {
                list_id: b.id,
                ..Default::default()
            })),
        )
        .await
        .expect("the query runs")
        .0;
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].record.id, deal.id);
    }

    /// Same clamp, second paginated route. `query_list_entries` ignores the struct's
    /// `limit` too.
    #[tokio::test]
    async fn querying_entries_clamps_a_hostile_limit() {
        let state = state();
        let list = state
            .store
            .create_list(&CreateListRequest {
                object_id: OBJ_DEAL.into(),
                name: "Big".into(),
                ..Default::default()
            })
            .await
            .unwrap();

        let page = query_list_entries(
            State(state.clone()),
            Path(list.id),
            Some(Json(ListEntryQuery {
                limit: Some(100_000),
                ..Default::default()
            })),
        )
        .await
        .expect("the query runs")
        .0;
        assert_eq!(page.limit, MAX_PAGE_SIZE);
    }

    /// `reorder_list_entries` returns `Ok(())` whether or not anything matched, so
    /// without the handler's pre-check a reorder against a deleted list answers 200
    /// and the panel believes its drag was saved.
    #[tokio::test]
    async fn reordering_an_unknown_list_is_a_404() {
        let state = state();
        let err = reorder_list_entries(
            State(state),
            Path("lst_nope".to_string()),
            Json(ReorderRequest { ids: Vec::new() }),
        )
        .await
        .expect_err("unknown list");
        assert!(matches!(err, ApiError::NotFound(_)), "got {err:?}");
    }

    /// Deleting a list removes the membership and the list's own fields — never the
    /// records. A list is a set; dropping the set must not drop its members.
    #[tokio::test]
    async fn deleting_a_list_keeps_its_records() {
        let state = state();
        let deal = seed_deal(&state, "Survivor", OPT_DEAL_STAGE_PROPOSAL).await;
        let list = state
            .store
            .create_list(&CreateListRequest {
                object_id: OBJ_DEAL.into(),
                name: "Temporary".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        state
            .store
            .add_list_entry(
                &list.id,
                &AddListEntryRequest {
                    record_id: deal.id.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();

        let _ = delete_list(State(state.clone()), Path(list.id.clone()))
            .await
            .expect("the list is deletable");

        assert!(state.store.get_list(&list.id).await.unwrap().is_none());
        assert!(
            state.store.get_record(&deal.id).await.unwrap().is_some(),
            "the record must outlive the list it was in"
        );
    }

    /// An unknown `?object_id=` filter is a 400 rather than an empty list: an empty
    /// array reads as "this object has no lists", which hides the typo that produced
    /// it.
    #[tokio::test]
    async fn an_unknown_object_filter_is_rejected_rather_than_returning_nothing() {
        let state = state();
        let err = list_lists(
            State(state.clone()),
            Query(ListScopeQuery {
                object_id: Some("obj_nope".into()),
            }),
        )
        .await
        .expect_err("unknown object filter");
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");

        // A blank value is the same as an absent one — a panel that always sends the
        // key must not be scoped to an object called "".
        let all = list_lists(
            State(state),
            Query(ListScopeQuery {
                object_id: Some("   ".into()),
            }),
        )
        .await
        .expect("a blank filter means no filter")
        .0;
        assert!(all["lists"].is_array());
    }

    /// A view saved against an object addressed by SLUG must store the canonical id,
    /// or `list_views` on the id would not find it.
    #[tokio::test]
    async fn creating_a_view_resolves_the_object_slug() {
        let state = state();
        let view = create_view(
            State(state.clone()),
            Path("deal".to_string()),
            Json(CreateViewRequest {
                name: "Closing this month".into(),
                kind: ViewKind::List,
                ..Default::default()
            }),
        )
        .await
        .expect("create the view")
        .0;
        assert_eq!(view.object_id, OBJ_DEAL);
        assert!(state
            .store
            .list_views(OBJ_DEAL)
            .await
            .unwrap()
            .iter()
            .any(|v| v.id == view.id));
    }

    /// An unknown object in the PATH is a 404, not the 500 the store's `bail!` would
    /// otherwise produce.
    #[tokio::test]
    async fn creating_a_view_on_an_unknown_object_is_a_404() {
        let state = state();
        let err = create_view(
            State(state),
            Path("obj_nope".to_string()),
            Json(CreateViewRequest {
                name: "Orphan".into(),
                ..Default::default()
            }),
        )
        .await
        .expect_err("unknown object");
        assert!(matches!(err, ApiError::NotFound(_)), "got {err:?}");
    }
}
