//! Activities and tasks — the unified per-record timeline, and the cross-object task
//! inbox that reads the same table from the other end.
//!
//! Paths here are MOUNT-LOCAL: `main` nests every `api::*` router under
//! `/api/crm`, so this file declares `/activities`, never `/api/crm/activities`.
//! axum is 0.7, so a path capture is `:activity_id` and not `{activity_id}`.
//!
//! Two rules shape almost everything below.
//!
//! **The store writes the audit trail; this router only reads it back.** A
//! `field_change` is written by `update_record` and a `stage_change` by a status move,
//! both inside the store's own transaction. They appear in every timeline read here,
//! they are refused by `create_activity`, and — the part the store does NOT enforce —
//! they are refused for edit and delete by [`guard_user_authored`]. That guard lives
//! here because `store::update_activity` / `store::delete_activity` are kind-blind: an
//! editable audit entry is worse than no audit entry at all, and `funnel_report`
//! reconstructs its stage paths from exactly those `stage_change` rows.
//!
//! **`limit` is a handler responsibility.** Every paginated store function takes
//! `limit`/`offset` as separate arguments and uses them verbatim; the `limit`/`offset`
//! fields on `ActivityQuery` / `TaskQuery` are wire-only and the store ignores them.
//! So each of the three list routes writes `state.config.clamp_limit(q.limit)` and
//! passes the result — forwarding `q.limit` into the struct compiles, looks right, and
//! has no ceiling.
//!
//! This module emits NO app events. `task.due` is raised by `main`'s sweep from
//! `store::claim_due_tasks`, which is what makes the announcement idempotent; a task
//! created or completed through here is not a due-date event.

use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Duration, FixedOffset, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::models::*;
use crate::state::AppState;

/// Build the timeline router. State is applied by `main`, which merges the six
/// `api::*` routers and calls `.with_state(state)` once.
pub fn routes() -> Router<AppState> {
    Router::new()
        // ── One record's timeline ──
        .route(
            "/records/:record_id/activities",
            get(list_record_activities).post(create_record_activity),
        )
        // ── The global feed + one entry ──
        //
        // `/activities/:activity_id/complete` is a SEPARATE route from its parent, not
        // a prefix match: Core's ext-proxy matcher requires an exact segment count, so
        // declaring the parent in the manifest does not admit this child either.
        .route("/activities", get(list_activities))
        .route(
            "/activities/:activity_id",
            get(get_activity)
                .patch(patch_activity)
                .delete(delete_activity),
        )
        .route("/activities/:activity_id/complete", post(complete_activity))
        // ── The task inbox ──
        .route("/tasks", get(list_tasks).post(create_task))
}

/// Alias for the module-function name the panel-side brief uses.
///
/// The foundation contract names every router module's entry point `routes()`, and the
/// per-module briefs name it `router()`. `main` is written by another agent reading one
/// of those two documents, and a missing symbol at merge time costs more than an alias
/// costs to carry. Delete whichever of the two nothing calls.
#[allow(dead_code)]
pub fn router() -> Router<AppState> {
    routes()
}

// ── Shared query shapes ────────────────────────────────────────────────────────

/// `GET /records/:record_id/activities`. The record comes from the path, so this
/// carries only the narrowing knobs.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct TimelineQuery {
    /// Comma-separated [`ActivityKind`] names, e.g. `?kinds=note,call`. Absent (or
    /// empty after parsing) means every kind, which is the point of a *unified*
    /// timeline.
    #[serde(default)]
    kinds: Option<String>,
    #[serde(default)]
    search: Option<String>,
    /// RFC-3339 bounds on `created_at`.
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

/// `GET /activities` — the same read, unscoped, plus the two filters only a
/// cross-record feed can use.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ActivityFeedQuery {
    #[serde(default)]
    record_id: Option<String>,
    /// Object id OR slug; the store resolves either and returns an empty page for an
    /// object that does not exist.
    #[serde(default)]
    object_id: Option<String>,
    #[serde(default)]
    kinds: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

/// Decode `?kinds=note,call` into the store's filter list.
///
/// Unknown names are DROPPED, never rejected: a filter chip from a newer panel must not
/// 400 an older sidecar. Dropping is safe here only because [`ActivityKind::parse`]
/// returns an `Option` — the tolerant [`ActivityKind::from_db`] would have coerced
/// "voicemail" into `note` and quietly filtered to the wrong kind.
///
/// A string in which nothing survives therefore behaves as "no kind filter". That is
/// the deliberate cost of forward-compatibility, and it is why the panel sends ids it
/// got from this app rather than free text.
fn parse_kinds(raw: Option<&str>) -> Vec<ActivityKind> {
    raw.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .filter_map(ActivityKind::parse)
            .collect()
    })
    .unwrap_or_default()
}

/// Turn a store `bool` "did anything change" into a 404, so a caller can tell a missing
/// row from a successful no-op.
fn require_hit(changed: bool, what: &str) -> ApiResult<()> {
    if changed {
        Ok(())
    } else {
        Err(ApiError::not_found(what))
    }
}

/// 404 for a record id that names nothing.
///
/// One indexed primary-key lookup on top of the timeline read, and worth it on BOTH
/// the GET and the POST: without it a typo'd id renders as "no activity yet" (a real
/// answer for a real record) and a note posts against a ghost. `get_record` returns
/// soft-deleted rows too, so a record in the trash still shows and accepts history.
async fn require_record(state: &AppState, record_id: &str) -> ApiResult<()> {
    if state.store.get_record(record_id).await?.is_none() {
        return Err(ApiError::not_found("record"));
    }
    Ok(())
}

/// Refuse an edit or a delete on an entry the store wrote itself.
///
/// `store::update_activity` and `store::delete_activity` are kind-blind by design —
/// they are also how a record cascade removes history — so this is the only thing
/// standing between a user and a rewritten audit trail.
fn guard_user_authored(activity: &Activity, verb: &str) -> ApiResult<()> {
    if activity.kind.is_user_authored() {
        return Ok(());
    }
    Err(ApiError::conflict(format!(
        "a \"{}\" entry is written automatically and cannot be {verb}",
        activity.kind.as_str()
    )))
}

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
    guard_user_authored(&existing, "edited")?;
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
    guard_user_authored(&existing, "deleted")?;
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
    if existing.kind != ActivityKind::Task {
        return Err(ApiError::conflict(format!(
            "only a task can be completed; \"{}\" is a {} entry",
            existing.title,
            existing.kind.as_str()
        )));
    }
    state
        .store
        .complete_task(&activity_id, completed)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("activity"))
}

// ── The task inbox ─────────────────────────────────────────────────────────────

/// The three lenses the inbox offers on top of `TaskFilter`.
///
/// Deliberately NOT a partition: a task due at 09:00 that it is now 15:00 is both
/// `overdue` and `today`, which is what every task list a user has ever used does.
/// `today` and `upcoming` are disjoint, so a two-section inbox rendering both cannot
/// show the same row twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TaskWindow {
    /// Not completed, `due_at` in the past.
    Overdue,
    /// Not completed, due at some point during the caller's local day.
    Today,
    /// Not completed, due on or after the caller's next local day.
    Upcoming,
}

impl TaskWindow {
    /// Which `TaskFilter` the window implies. Every window is about work still to do,
    /// so none of them is `Completed` or `All`.
    const fn filter(self) -> TaskFilter {
        match self {
            Self::Overdue => TaskFilter::Overdue,
            Self::Today | Self::Upcoming => TaskFilter::Open,
        }
    }
}

/// Largest real UTC offset, in minutes (UTC+14, Kiritimati). A caller-supplied offset
/// is clamped to `±` this rather than rejected: a wrong clock should shift the day
/// boundary, not empty the inbox.
const MAX_UTC_OFFSET_MINUTES: i32 = 14 * 60;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TaskListQuery {
    /// `Option`, unlike `TaskQuery::filter`, precisely so an absent filter is
    /// distinguishable from an explicit `open` — otherwise the `window` conflict check
    /// below would fire on every plain `?window=today`.
    #[serde(default)]
    filter: Option<TaskFilter>,
    #[serde(default)]
    window: Option<TaskWindow>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    record_id: Option<String>,
    #[serde(default)]
    object_id: Option<String>,
    /// Inclusive RFC-3339 bounds on `due_at`.
    #[serde(default)]
    due_before: Option<String>,
    #[serde(default)]
    due_after: Option<String>,
    /// Minutes EAST of UTC — i.e. `-new Date().getTimezoneOffset()`. Decides where
    /// "today" starts and ends. Absent means UTC, which is only right for a caller
    /// actually in UTC; the panel always sends it.
    #[serde(default)]
    utc_offset_minutes: Option<i32>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

/// Expand a window into `(due_after, due_before)`.
///
/// Takes `now` rather than reading the clock so it is testable against fixed instants;
/// the handler passes `Utc::now()`.
///
/// Both bounds are stamped with the same `to_rfc3339_opts(Millis, true)` shape as
/// `models::now_rfc3339`, which is load-bearing: the store range-scans `due_at` as
/// TEXT, so a bound of a different width (seconds precision, or a `+00:00` suffix)
/// would compare lexicographically against a differently-shaped column and silently
/// select the wrong rows.
fn task_window_bounds(
    now: DateTime<Utc>,
    utc_offset_minutes: i32,
    window: TaskWindow,
) -> (Option<String>, Option<String>) {
    match window {
        // Left entirely to the store, whose `TaskFilter::Overdue` is "not completed and
        // `due_at <= now`". Deriving a bound here would be a second definition of the
        // word, free to drift from the one `summary()` reports on the dashboard.
        TaskWindow::Overdue => (None, None),
        TaskWindow::Today => {
            let start = local_day_start(now, utc_offset_minutes, 0);
            // The store's `due_before` is INCLUSIVE, so the end is one millisecond
            // before the next local midnight — timestamps carry millisecond precision,
            // so that is exact rather than a fudge.
            let end = local_day_start(now, utc_offset_minutes, 1) - Duration::milliseconds(1);
            (Some(stamp(start)), Some(stamp(end)))
        }
        TaskWindow::Upcoming => (
            Some(stamp(local_day_start(now, utc_offset_minutes, 1))),
            None,
        ),
    }
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Midnight, `days_ahead` local days from `now`, expressed as an instant in UTC.
///
/// A fixed offset has no DST, so "add a day then take the local midnight" is exact.
fn local_day_start(now: DateTime<Utc>, utc_offset_minutes: i32, days_ahead: i64) -> DateTime<Utc> {
    let minutes = utc_offset_minutes.clamp(-MAX_UTC_OFFSET_MINUTES, MAX_UTC_OFFSET_MINUTES);
    let Some(zone) = FixedOffset::east_opt(minutes * 60) else {
        return now;
    };
    let local = now.with_timezone(&zone) + Duration::days(days_ahead);
    local
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|midnight| midnight.and_local_timezone(zone).single())
        .map(|at| at.with_timezone(&Utc))
        // Unreachable for a fixed offset (no gap, no fold), but degrading to `now`
        // keeps a hypothetical failure to a narrow window rather than a panic.
        .unwrap_or(now)
}

/// The cross-object task inbox.
///
/// Undated tasks sort last — the store's ordering, not re-derived here.
#[utoipa::path(
    get,
    path = "/api/crm/tasks",
    tag = "CRM",
    summary = "the cross-object task inbox — what is open, overdue, due today or coming up, across every record.",
    params(
        ("filter" = Option<String>, Query, description = "`open`, `completed`, `overdue`, or `all`. Mutually exclusive with `window`."),
        ("window" = Option<String>, Query, description = "Shorthand for a filter plus a date range: `overdue`, `today`, or `upcoming`. Do not send it together with `filter` / `due_after` / `due_before`."),
        ("assignee" = Option<String>, Query, description = "Restrict to one person's tasks."),
        ("record_id" = Option<String>, Query, description = "Restrict to one record's tasks."),
        ("object_id" = Option<String>, Query, description = "Restrict to one object's records, by id or slug."),
        ("due_before" = Option<String>, Query, description = "Inclusive RFC-3339 upper bound on the due date."),
        ("due_after" = Option<String>, Query, description = "Inclusive RFC-3339 lower bound on the due date."),
        ("utc_offset_minutes" = Option<i32>, Query, description = "Minutes EAST of UTC, deciding where the caller's `today` starts and ends. Defaults to UTC."),
        ("limit" = Option<usize>, Query, description = "Page size."),
        ("offset" = Option<usize>, Query, description = "How many tasks to skip.")
    ),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn list_tasks(
    State(state): State<AppState>,
    Query(q): Query<TaskListQuery>,
) -> ApiResult<Json<ActivityPage>> {
    let (filter, due_after, due_before) = match q.window {
        Some(window) => {
            // `window` IS `filter` + `due_after` + `due_before`. Accepting both and
            // picking a winner would make one of the two silently inert, which is the
            // sort of thing that gets debugged as "the date filter doesn't work".
            if q.filter.is_some() || q.due_before.is_some() || q.due_after.is_some() {
                return Err(ApiError::bad_request(
                    "`window` is shorthand for `filter` + `due_after` + `due_before`; send one or the other, not both",
                ));
            }
            let (after, before) =
                task_window_bounds(Utc::now(), q.utc_offset_minutes.unwrap_or(0), window);
            (window.filter(), after, before)
        }
        None => (q.filter.unwrap_or_default(), q.due_after, q.due_before),
    };
    let limit = state.config.clamp_limit(q.limit);
    let offset = q.offset.unwrap_or(0);
    let query = TaskQuery {
        filter,
        assignee: q.assignee.filter(|a| !a.trim().is_empty()),
        record_id: q.record_id.filter(|r| !r.trim().is_empty()),
        object_id: q.object_id.filter(|o| !o.trim().is_empty()),
        due_before,
        due_after,
        limit: None,
        offset: None,
    };
    Ok(Json(state.store.list_tasks(&query, limit, offset).await?))
}

/// A task created from the task list rather than from a record's timeline.
///
/// Its own body type rather than `CreateActivityRequest` because that struct's `kind`
/// is a REQUIRED field, so `POST /tasks {"title":"call Bob"}` would fail to
/// deserialize on a route whose whole contract is that the kind is already decided.
// `pub(crate)` only so `api::mod`'s `components(schemas(...))` list can name it.
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub(crate) struct CreateTaskBody {
    /// The record the task hangs off. Omit for a standalone task — one attached to no
    /// record, which the record-scoped route cannot express.
    #[serde(default)]
    pub record_id: Option<String>,
    /// One line saying what needs doing.
    #[serde(default)]
    pub title: String,
    /// Longer detail, if the title is not enough.
    #[serde(default)]
    pub body: Option<String>,
    /// Who owns the task. Free text.
    #[serde(default)]
    pub assignee: Option<String>,
    /// When it is due. RFC-3339, or a bare `YYYY-MM-DD`.
    #[serde(default)]
    pub due_at: Option<String>,
    /// Who is creating it, for the audit trail.
    #[serde(default)]
    pub author: Option<String>,
    /// Arbitrary JSON the caller wants carried alongside the task.
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[utoipa::path(
    post,
    path = "/api/crm/tasks",
    tag = "CRM",
    summary = "create a task, with or without a record attached.",
    request_body = CreateTaskBody,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn create_task(
    State(state): State<AppState>,
    Json(body): Json<CreateTaskBody>,
) -> ApiResult<Json<Activity>> {
    // Stricter than the timeline route, which accepts a title-less note: a task's title
    // is the only thing an inbox row renders, so a blank one is an invisible task.
    if body.title.trim().is_empty() {
        return Err(ApiError::bad_request("a task needs a title"));
    }
    let req = CreateActivityRequest {
        record_id: body.record_id.filter(|r| !r.trim().is_empty()),
        kind: ActivityKind::Task,
        title: body.title,
        body: body.body,
        assignee: body.assignee,
        due_at: body.due_at,
        author: body.author,
        metadata: body.metadata,
    };
    // A `record_id` naming nothing is a rejected BODY field, so it stays the store's
    // 422 — unlike the path-supplied id on the record route, which is a 404.
    match state.store.create_activity(&req).await? {
        Ok(activity) => Ok(Json(activity)),
        Err(errors) => Err(ApiError::validation(errors)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MAX_PAGE_SIZE;

    fn state() -> AppState {
        AppState::in_memory().expect("in-memory state")
    }

    /// A fixed instant so the window arithmetic is deterministic. 03:30 UTC is chosen
    /// because it falls on the PREVIOUS local day for any western offset, which is
    /// exactly the case a UTC-only implementation gets wrong.
    fn at(raw: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(raw)
            .expect("a valid fixture instant")
            .with_timezone(&Utc)
    }

    async fn seeded_deal(state: &AppState) -> Record {
        let mut values = ValueBag::new();
        values.insert("name".into(), json!("Acme expansion"));
        values.insert("stage".into(), json!("Lead"));
        state
            .store
            .create_record(
                "deal",
                &CreateRecordRequest {
                    values,
                    created_by: None,
                },
            )
            .await
            .expect("no infrastructure error")
            .expect("seed values are valid")
    }

    async fn note_on(state: &AppState, record_id: &str, title: &str) -> Activity {
        create_record_activity(
            State(state.clone()),
            Path(record_id.to_string()),
            Json(CreateActivityRequest {
                kind: ActivityKind::Note,
                title: title.to_string(),
                ..CreateActivityRequest::default()
            }),
        )
        .await
        .expect("a titled note is a valid entry")
        .0
    }

    /// Building the router is what validates every path pattern. Two routes that
    /// conflict panic HERE, at `Router::new().route(...)`, not at `cargo check`.
    #[test]
    fn the_router_builds_with_every_route_registered() {
        let _routes = routes();
        let _alias = router();
    }

    /// A chip from a newer panel must not 400 an older sidecar, so an unrecognized kind
    /// is dropped and its recognized siblings still apply. The reason this is safe is
    /// `ActivityKind::parse`: the tolerant `from_db` would have turned "voicemail" into
    /// `note` and filtered to a kind nobody asked for.
    #[test]
    fn unknown_kinds_are_dropped_and_never_coerced() {
        assert_eq!(parse_kinds(None), Vec::<ActivityKind>::new());
        assert_eq!(parse_kinds(Some("")), Vec::<ActivityKind>::new());
        assert_eq!(
            parse_kinds(Some(" note , voicemail ,call")),
            vec![ActivityKind::Note, ActivityKind::Call]
        );
        // The whole point: nothing became `note` by accident.
        assert_eq!(parse_kinds(Some("voicemail")), Vec::<ActivityKind>::new());
        assert_eq!(
            parse_kinds(Some("stage_change")),
            vec![ActivityKind::StageChange]
        );
    }

    /// The bounds must be byte-comparable with the `due_at` column, and the local
    /// offset must actually move the day boundary — a UTC-only "today" is wrong for
    /// seven hours out of every day in California.
    #[test]
    fn task_window_bounds_track_the_local_day_and_match_the_stored_stamp_shape() {
        let now = at("2026-08-10T03:30:00Z");

        let (after, before) = task_window_bounds(now, 0, TaskWindow::Today);
        assert_eq!(after.as_deref(), Some("2026-08-10T00:00:00.000Z"));
        assert_eq!(before.as_deref(), Some("2026-08-10T23:59:59.999Z"));
        // Same width and suffix as `now_rfc3339()`, or the TEXT range scan compares
        // strings of different shapes.
        assert_eq!(after.as_deref().unwrap().len(), now_rfc3339().len());

        // UTC-7: 03:30Z is still the 9th locally, so "today" is the 9th, shifted by
        // exactly seven hours.
        let (after, before) = task_window_bounds(now, -420, TaskWindow::Today);
        assert_eq!(after.as_deref(), Some("2026-08-09T07:00:00.000Z"));
        assert_eq!(before.as_deref(), Some("2026-08-10T06:59:59.999Z"));

        // `upcoming` starts where `today` ends, so a two-section inbox shows no row
        // twice.
        let (after, before) = task_window_bounds(now, -420, TaskWindow::Upcoming);
        assert_eq!(after.as_deref(), Some("2026-08-10T07:00:00.000Z"));
        assert_eq!(before, None);

        // `overdue` is the store's own definition and gets no bound from here.
        assert_eq!(
            task_window_bounds(now, -420, TaskWindow::Overdue),
            (None, None)
        );
    }

    /// A nonsense offset shifts the boundary to the extreme, it does not panic and does
    /// not degrade to `now` (which would make "today" start at an arbitrary moment).
    #[test]
    fn an_absurd_utc_offset_is_clamped_rather_than_trusted() {
        let now = at("2026-08-10T03:30:00Z");
        let (after, _) = task_window_bounds(now, 100_000, TaskWindow::Today);
        // Clamped to UTC+14, where 03:30Z is 17:30 on the 10th; local midnight is
        // 10:00Z on the 9th.
        assert_eq!(after.as_deref(), Some("2026-08-09T10:00:00.000Z"));
        // Clamped the other way to UTC-14, where 03:30Z is 13:30 on the NINTH — the
        // offset is big enough to cross back over midnight — so local midnight is
        // 14:00Z on the 9th, not the 10th.
        let (after, _) = task_window_bounds(now, -100_000, TaskWindow::Today);
        assert_eq!(after.as_deref(), Some("2026-08-09T14:00:00.000Z"));
    }

    /// `window` and the raw bounds are two spellings of one thing. Accepting both would
    /// make one of them silently inert.
    #[tokio::test]
    async fn window_and_explicit_bounds_are_mutually_exclusive() {
        let err = list_tasks(
            State(state()),
            Query(TaskListQuery {
                window: Some(TaskWindow::Today),
                due_before: Some("2026-01-01T00:00:00.000Z".into()),
                ..TaskListQuery::default()
            }),
        )
        .await
        .expect_err("both spellings at once must be refused");
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    /// The regression the `Option<TaskFilter>` in `TaskListQuery` exists to prevent: if
    /// the filter were `TaskFilter` with a serde default, it would always be `Some`-like
    /// and the conflict check above would fire on every plain window query.
    #[tokio::test]
    async fn a_plain_window_query_is_accepted() {
        let state = state();
        let now = Utc::now();
        let due_today = create_task(
            State(state.clone()),
            Json(CreateTaskBody {
                title: "Call Bob".into(),
                due_at: Some(stamp(now)),
                ..CreateTaskBody::default()
            }),
        )
        .await
        .expect("a titled task is valid")
        .0;
        let due_next_week = create_task(
            State(state.clone()),
            Json(CreateTaskBody {
                title: "Renewal review".into(),
                due_at: Some(stamp(now + Duration::days(7))),
                ..CreateTaskBody::default()
            }),
        )
        .await
        .expect("a titled task is valid")
        .0;

        let today = list_tasks(
            State(state.clone()),
            Query(TaskListQuery {
                window: Some(TaskWindow::Today),
                ..TaskListQuery::default()
            }),
        )
        .await
        .expect("a window alone is a well-formed query")
        .0;
        let ids: Vec<&str> = today.items.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec![due_today.id.as_str()]);

        let upcoming = list_tasks(
            State(state.clone()),
            Query(TaskListQuery {
                window: Some(TaskWindow::Upcoming),
                ..TaskListQuery::default()
            }),
        )
        .await
        .unwrap()
        .0;
        let ids: Vec<&str> = upcoming.items.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec![due_next_week.id.as_str()]);
    }

    /// The audit-trail guard. `store::update_activity` and `store::delete_activity` are
    /// kind-blind, so without this a user can rewrite the history of a deal — and
    /// deleting a `stage_change` is not merely cosmetic: `funnel_report` reconstructs
    /// every stage path in memory from exactly those rows, so one delete silently
    /// changes what the pipeline report says happened.
    #[tokio::test]
    async fn entries_the_store_wrote_itself_cannot_be_edited_or_deleted() {
        let state = state();
        let deal = seeded_deal(&state).await;

        let mut moved = ValueBag::new();
        moved.insert("stage".into(), json!("Proposal"));
        state
            .store
            .update_record(
                &deal.id,
                &UpdateRecordRequest {
                    values: moved,
                    mode: UpdateMode::Merge,
                },
            )
            .await
            .expect("no infrastructure error")
            .expect("a known option label is valid")
            .expect("the record exists");

        let automatic = list_record_activities(
            State(state.clone()),
            Path(deal.id.clone()),
            Query(TimelineQuery {
                kinds: Some("stage_change,field_change".into()),
                ..TimelineQuery::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(
            !automatic.items.is_empty(),
            "moving a status field must leave an automatic entry to protect"
        );

        for entry in &automatic.items {
            let err = patch_activity(
                State(state.clone()),
                Path(entry.id.clone()),
                Json(UpdateActivityRequest {
                    title: Some("never happened".into()),
                    ..UpdateActivityRequest::default()
                }),
            )
            .await
            .expect_err("an automatic entry must not be editable");
            assert!(matches!(err, ApiError::Conflict(_)));

            let err = delete_activity(State(state.clone()), Path(entry.id.clone()))
                .await
                .expect_err("an automatic entry must not be deletable");
            assert!(matches!(err, ApiError::Conflict(_)));
        }

        // Positive control: the guard must not have locked the whole timeline.
        let note = note_on(&state, &deal.id, "Left a voicemail").await;
        let edited = patch_activity(
            State(state.clone()),
            Path(note.id.clone()),
            Json(UpdateActivityRequest {
                title: Some("Spoke to Dana".into()),
                ..UpdateActivityRequest::default()
            }),
        )
        .await
        .expect("an authored note is editable")
        .0;
        assert_eq!(edited.title, "Spoke to Dana");
        delete_activity(State(state.clone()), Path(note.id))
            .await
            .expect("an authored note is deletable");
    }

    /// `store::complete_task` scopes its UPDATE to `kind = 'task'` and then returns
    /// whatever row it found, so without this guard completing a note answers 200 with
    /// an unchanged note — a success response for work that did not happen.
    #[tokio::test]
    async fn completing_something_that_is_not_a_task_is_refused_not_ignored() {
        let state = state();
        let deal = seeded_deal(&state).await;
        let note = note_on(&state, &deal.id, "Kickoff summary").await;

        let err = complete_activity(State(state.clone()), Path(note.id.clone()), None)
            .await
            .expect_err("a note has nothing to complete");
        assert!(matches!(err, ApiError::Conflict(_)));

        let task = create_task(
            State(state.clone()),
            Json(CreateTaskBody {
                record_id: Some(deal.id.clone()),
                title: "Send the proposal".into(),
                ..CreateTaskBody::default()
            }),
        )
        .await
        .unwrap()
        .0;

        // A bodiless click completes. The derived `Default` for `CompleteTaskRequest`
        // is `completed: false`, so a handler written with `unwrap_or_default()` would
        // pass this assertion's opposite.
        let done = complete_activity(State(state.clone()), Path(task.id.clone()), None)
            .await
            .unwrap()
            .0;
        assert!(done.completed_at.is_some());

        let reopened = complete_activity(
            State(state.clone()),
            Path(task.id),
            Some(Json(CompleteTaskRequest { completed: false })),
        )
        .await
        .unwrap()
        .0;
        assert!(reopened.completed_at.is_none());
    }

    /// The store uses `limit` verbatim, so the ceiling exists only if the handler
    /// applies it. Forwarding the caller's number compiles and looks right.
    #[tokio::test]
    async fn every_list_route_clamps_the_callers_limit() {
        let state = state();
        let deal = seeded_deal(&state).await;
        note_on(&state, &deal.id, "One").await;

        let timeline = list_record_activities(
            State(state.clone()),
            Path(deal.id.clone()),
            Query(TimelineQuery {
                limit: Some(100_000),
                ..TimelineQuery::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(timeline.limit, MAX_PAGE_SIZE);

        let feed = list_activities(
            State(state.clone()),
            Query(ActivityFeedQuery {
                limit: Some(100_000),
                ..ActivityFeedQuery::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(feed.limit, MAX_PAGE_SIZE);

        let tasks = list_tasks(
            State(state.clone()),
            Query(TaskListQuery {
                limit: Some(100_000),
                ..TaskListQuery::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(tasks.limit, MAX_PAGE_SIZE);
    }

    /// A path segment that names nothing is a missing resource, not a rejected value —
    /// otherwise a typo'd record id renders as "no activity yet", which is a real
    /// answer for a real record.
    #[tokio::test]
    async fn a_timeline_on_an_unknown_record_is_a_404_not_an_empty_page() {
        let state = state();
        let err = list_record_activities(
            State(state.clone()),
            Path("rec_nope".into()),
            Query(TimelineQuery::default()),
        )
        .await
        .expect_err("an unknown record has no timeline");
        assert!(matches!(err, ApiError::NotFound(_)));

        let err = create_record_activity(
            State(state.clone()),
            Path("rec_nope".into()),
            Json(CreateActivityRequest {
                kind: ActivityKind::Note,
                title: "orphan".into(),
                ..CreateActivityRequest::default()
            }),
        )
        .await
        .expect_err("a note must not attach to a ghost");
        assert!(matches!(err, ApiError::NotFound(_)));
    }

    /// An entry with neither a title nor a body renders as a blank row forever. A note
    /// carried only by its body is fine, though — that is how quick notes are written.
    #[tokio::test]
    async fn an_entirely_empty_entry_is_refused_but_a_body_only_note_is_not() {
        let state = state();
        let deal = seeded_deal(&state).await;

        let err = create_record_activity(
            State(state.clone()),
            Path(deal.id.clone()),
            Json(CreateActivityRequest {
                kind: ActivityKind::Note,
                title: "   ".into(),
                ..CreateActivityRequest::default()
            }),
        )
        .await
        .expect_err("nothing to render");
        assert!(matches!(err, ApiError::BadRequest(_)));

        let body_only = create_record_activity(
            State(state.clone()),
            Path(deal.id.clone()),
            Json(CreateActivityRequest {
                kind: ActivityKind::Note,
                body: Some("They want SSO before signing.".into()),
                ..CreateActivityRequest::default()
            }),
        )
        .await
        .expect("a body is content")
        .0;
        assert_eq!(body_only.kind, ActivityKind::Note);

        // A task, by contrast, is only ever rendered by its title.
        let err = create_task(
            State(state.clone()),
            Json(CreateTaskBody {
                body: Some("no title".into()),
                ..CreateTaskBody::default()
            }),
        )
        .await
        .expect_err("an inbox row needs a title");
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    /// `POST /tasks` decides the kind; the caller cannot post a note through it, and —
    /// the reason for the bespoke body type — need not send a `kind` at all, which
    /// `CreateActivityRequest` (whose `kind` has no serde default) would have rejected.
    #[tokio::test]
    async fn a_standalone_task_needs_neither_a_record_nor_a_kind() {
        let state = state();
        let parsed: CreateTaskBody =
            serde_json::from_value(json!({ "title": "Draft the renewal email" }))
                .expect("a bare title must deserialize");
        let task = create_task(State(state.clone()), Json(parsed))
            .await
            .expect("a standalone task is valid")
            .0;
        assert_eq!(task.kind, ActivityKind::Task);
        assert_eq!(task.record_id, None);

        let inbox = list_tasks(State(state.clone()), Query(TaskListQuery::default()))
            .await
            .unwrap()
            .0;
        assert_eq!(inbox.total, 1, "the default filter is `open`");
    }

    /// The feed's own filters, which the record-scoped route cannot express.
    #[tokio::test]
    async fn the_global_feed_filters_by_object_and_assignee() {
        let state = state();
        let deal = seeded_deal(&state).await;
        note_on(&state, &deal.id, "Deal note").await;
        create_task(
            State(state.clone()),
            Json(CreateTaskBody {
                record_id: Some(deal.id.clone()),
                title: "Chase legal".into(),
                assignee: Some("dana".into()),
                ..CreateTaskBody::default()
            }),
        )
        .await
        .unwrap();

        // By object SLUG, not just id — the store resolves either.
        let by_object = list_activities(
            State(state.clone()),
            Query(ActivityFeedQuery {
                object_id: Some("deal".into()),
                kinds: Some("note,task".into()),
                ..ActivityFeedQuery::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(by_object.total, 2);

        let by_assignee = list_activities(
            State(state.clone()),
            Query(ActivityFeedQuery {
                assignee: Some("dana".into()),
                ..ActivityFeedQuery::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(by_assignee.total, 1);

        // A blank parameter is the same as an absent one — a panel that always sends
        // the key must not end up filtering on the empty string.
        let blank = list_activities(
            State(state.clone()),
            Query(ActivityFeedQuery {
                assignee: Some("  ".into()),
                object_id: Some(String::new()),
                ..ActivityFeedQuery::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(blank.total >= 2);
    }
}
