//! One record's timeline and the global feed.

use super::*;

// ── One record's timeline ──────────────────────────────────────────────────────

/// The record's history, newest first, paginated.
//
// `limit`/`offset` are left `None` on the `ActivityQuery` on purpose: they are
// wire-only fields the store ignores, and populating them would read as though they
// were doing the paging.
#[utoipa::path(
    get,
    path = "/api/crm/records/{record_id}/activities",
    tag = "CRM",
    summary = "read one record's timeline — its notes, calls, meetings, tasks and the automatic field/stage-change entries, newest first.",
    params(
        ("record_id" = String, Path, description = "The record's id."),
        ("kinds" = Option<String>, Query, description = "Comma-separated kinds to keep: `note`, `task`, `call`, `meeting`, `field_change`, `stage_change`. Omit for everything."),
        ("search" = Option<String>, Query, description = "Free text over the entries' titles and bodies."),
        ("since" = Option<String>, Query, description = "RFC-3339 lower bound on when the entry was written."),
        ("until" = Option<String>, Query, description = "RFC-3339 upper bound on when the entry was written."),
        ("limit" = Option<usize>, Query, description = "Page size."),
        ("offset" = Option<usize>, Query, description = "How many entries to skip.")
    ),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn list_record_activities(
    State(state): State<AppState>,
    Path(record_id): Path<String>,
    Query(q): Query<TimelineQuery>,
) -> ApiResult<Json<ActivityPage>> {
    require_record(&state, &record_id).await?;
    let limit = state.config.clamp_limit(q.limit);
    let offset = q.offset.unwrap_or(0);
    let query = ActivityQuery {
        record_id: Some(record_id),
        object_id: None,
        kinds: parse_kinds(q.kinds.as_deref()),
        assignee: None,
        search: q.search,
        since: q.since,
        until: q.until,
        limit: None,
        offset: None,
    };
    Ok(Json(
        state.store.query_activities(&query, limit, offset).await?,
    ))
}

/// Append a note, call or meeting to a record.
//
// The two automatic kinds are refused by `store::create_activity` as a field-level
// rejection (422 with `fields[0].field_slug == "kind"`), NOT pre-checked here: one
// definition of "user-authored" on the write path, and the panel gets the same shaped
// error it gets for a bad `due_at`.
// The body's own `record_id` is documented but IGNORED — the path wins, because this
// URL is what says whose timeline is being appended to.
#[utoipa::path(
    post,
    path = "/api/crm/records/{record_id}/activities",
    tag = "CRM",
    summary = "add a note, call, meeting or task to one record's timeline.",
    params(("record_id" = String, Path, description = "The record's id. Wins over any `record_id` in the body.")),
    request_body = CreateActivityRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn create_record_activity(
    State(state): State<AppState>,
    Path(record_id): Path<String>,
    Json(body): Json<CreateActivityRequest>,
) -> ApiResult<Json<Activity>> {
    require_record(&state, &record_id).await?;
    // A note's content is its `body`, a call's is often only its `title` — so either
    // one alone is a real entry and neither is not. Rejected here rather than in the
    // store because an empty entry is a UI slip, not a value that failed validation.
    if body.title.trim().is_empty() && body.body.as_deref().unwrap_or("").trim().is_empty() {
        return Err(ApiError::bad_request(
            "a timeline entry needs a title or a body",
        ));
    }
    // The path wins over any `record_id` in the payload: this URL says which record's
    // timeline is being appended to.
    let req = CreateActivityRequest {
        record_id: Some(record_id),
        ..body
    };
    match state.store.create_activity(&req).await? {
        Ok(activity) => Ok(Json(activity)),
        Err(errors) => Err(ApiError::validation(errors)),
    }
}

// ── The global feed ────────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/crm/activities",
    tag = "CRM",
    summary = "read the CRM's whole activity feed across every record — what has been happening lately, optionally narrowed to one record, object, kind or person.",
    params(
        ("record_id" = Option<String>, Query, description = "Restrict to one record's entries."),
        ("object_id" = Option<String>, Query, description = "Restrict to one object's records, by id or slug."),
        ("kinds" = Option<String>, Query, description = "Comma-separated kinds: `note`, `task`, `call`, `meeting`, `field_change`, `stage_change`."),
        ("assignee" = Option<String>, Query, description = "Restrict to entries assigned to this person."),
        ("search" = Option<String>, Query, description = "Free text over titles and bodies."),
        ("since" = Option<String>, Query, description = "RFC-3339 lower bound on when the entry was written."),
        ("until" = Option<String>, Query, description = "RFC-3339 upper bound on when the entry was written."),
        ("limit" = Option<usize>, Query, description = "Page size."),
        ("offset" = Option<usize>, Query, description = "How many entries to skip.")
    ),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn list_activities(
    State(state): State<AppState>,
    Query(q): Query<ActivityFeedQuery>,
) -> ApiResult<Json<ActivityPage>> {
    let limit = state.config.clamp_limit(q.limit);
    let offset = q.offset.unwrap_or(0);
    let query = ActivityQuery {
        record_id: q.record_id.filter(|r| !r.trim().is_empty()),
        object_id: q.object_id.filter(|o| !o.trim().is_empty()),
        kinds: parse_kinds(q.kinds.as_deref()),
        assignee: q.assignee.filter(|a| !a.trim().is_empty()),
        search: q.search,
        since: q.since,
        until: q.until,
        limit: None,
        offset: None,
    };
    Ok(Json(
        state.store.query_activities(&query, limit, offset).await?,
    ))
}

#[utoipa::path(
    get,
    path = "/api/crm/activities/{activity_id}",
    tag = "CRM",
    summary = "read one timeline entry in full.",
    params(("activity_id" = String, Path, description = "The entry's id.")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn get_activity(
    State(state): State<AppState>,
    Path(activity_id): Path<String>,
) -> ApiResult<Json<Activity>> {
    state
        .store
        .get_activity(&activity_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("activity"))
}

/// Edit an authored entry, and complete/reopen a task via `completed`.
///
/// Note the merge asymmetry the store imposes and this route inherits: an explicit
/// `""` CLEARS `due_at`, but `body`, `assignee` and `metadata` fall back to the
/// existing value when absent AND when null, so they cannot be cleared through here.
#[utoipa::path(
    patch,
    path = "/api/crm/activities/{activity_id}",
    tag = "CRM",
    summary = "edit a timeline entry — its text, its assignee, its due date, or whether a task is done.",
    params(("activity_id" = String, Path, description = "The entry's id.")),
    request_body = UpdateActivityRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn patch_activity(
    State(state): State<AppState>,
    Path(activity_id): Path<String>,
    Json(body): Json<UpdateActivityRequest>,
) -> ApiResult<Json<Activity>> {
    let existing = state
        .store
        .get_activity(&activity_id)
        .await?
        .ok_or_else(|| ApiError::not_found("activity"))?;
    guard_user_authored(&existing, EntryChange::Edit)?;
    match state.store.update_activity(&activity_id, &body).await? {
        Ok(Some(activity)) => Ok(Json(activity)),
        // Deleted between the read and the write. Rare, but reporting it as a 404 is
        // the only honest answer.
        Ok(None) => Err(ApiError::not_found("activity")),
        Err(errors) => Err(ApiError::validation(errors)),
    }
}

#[utoipa::path(
    delete,
    path = "/api/crm/activities/{activity_id}",
    tag = "CRM",
    summary = "delete a timeline entry.",
    params(("activity_id" = String, Path, description = "The entry's id.")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn delete_activity(
    State(state): State<AppState>,
    Path(activity_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let existing = state
        .store
        .get_activity(&activity_id)
        .await?
        .ok_or_else(|| ApiError::not_found("activity"))?;
    guard_user_authored(&existing, EntryChange::Delete)?;
    require_hit(state.store.delete_activity(&activity_id).await?, "activity")?;
    Ok(Json(json!({ "ok": true })))
}

/// Complete or reopen a task.
//
// Guarded on kind BEFORE the write, because `store::complete_task` scopes its UPDATE
// to `kind = 'task'` and then returns the row it found either way — so a call against
// a note would answer 200 with an untouched note and look exactly like success.
// Plain type, not `Option<CompleteTaskRequest>` — an optional request body renders as
// a nullable wrapper that buries the one field a caller might send.
#[utoipa::path(
    post,
    path = "/api/crm/activities/{activity_id}/complete",
    tag = "CRM",
    summary = "tick a task off, or reopen it. Only entries of kind `task` can be completed.",
    params(("activity_id" = String, Path, description = "The task's id.")),
    request_body = CompleteTaskRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn complete_activity(
    State(state): State<AppState>,
    Path(activity_id): Path<String>,
    body: Option<Json<CompleteTaskRequest>>,
) -> ApiResult<Json<Activity>> {
    // The body is optional: a checkbox click sends no payload. Spelled out rather than
    // `unwrap_or_default()` because `CompleteTaskRequest`'s DERIVED default is
    // `completed: false` — only its SERDE default is `true` — so the tidier form would
    // silently reopen every task the user ticked.
    let completed = body.map_or(true, |Json(b)| b.completed);
    let existing = state
        .store
        .get_activity(&activity_id)
        .await?
        .ok_or_else(|| ApiError::not_found("activity"))?;
    if let Some(refusal) = not_a_task(&existing.title, existing.kind) {
        return Err(refusal);
    }
    state
        .store
        .complete_task(&activity_id, completed)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("activity"))
}
