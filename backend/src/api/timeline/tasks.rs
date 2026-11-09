//! The task inbox.

use super::*;

// ── The task inbox ─────────────────────────────────────────────────────────────

/// The three lenses the inbox offers on top of `TaskFilter`.
///
/// Deliberately NOT a partition: a task due at 09:00 that it is now 15:00 is both
/// `overdue` and `today`, which is what every task list a user has ever used does.
/// `today` and `upcoming` are disjoint, so a two-section inbox rendering both cannot
/// show the same row twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TaskWindow {
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
pub(super) const MAX_UTC_OFFSET_MINUTES: i32 = 14 * 60;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TaskListQuery {
    /// `Option`, unlike `TaskQuery::filter`, precisely so an absent filter is
    /// distinguishable from an explicit `open` — otherwise the `window` conflict check
    /// below would fire on every plain `?window=today`.
    #[serde(default)]
    pub(super) filter: Option<TaskFilter>,
    #[serde(default)]
    pub(super) window: Option<TaskWindow>,
    #[serde(default)]
    pub(super) assignee: Option<String>,
    #[serde(default)]
    pub(super) record_id: Option<String>,
    #[serde(default)]
    pub(super) object_id: Option<String>,
    /// Inclusive RFC-3339 bounds on `due_at`.
    #[serde(default)]
    pub(super) due_before: Option<String>,
    #[serde(default)]
    pub(super) due_after: Option<String>,
    /// Minutes EAST of UTC — i.e. `-new Date().getTimezoneOffset()`. Decides where
    /// "today" starts and ends. Absent means UTC, which is only right for a caller
    /// actually in UTC; the panel always sends it.
    #[serde(default)]
    pub(super) utc_offset_minutes: Option<i32>,
    #[serde(default)]
    pub(super) limit: Option<usize>,
    #[serde(default)]
    pub(super) offset: Option<usize>,
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
pub(super) fn task_window_bounds(
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

pub(super) fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Midnight, `days_ahead` local days from `now`, expressed as an instant in UTC.
///
/// A fixed offset has no DST, so "add a day then take the local midnight" is exact.
pub(super) fn local_day_start(
    now: DateTime<Utc>,
    utc_offset_minutes: i32,
    days_ahead: i64,
) -> DateTime<Utc> {
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
