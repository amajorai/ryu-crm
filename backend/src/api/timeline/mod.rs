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

mod entries;
mod refusals;
mod tasks;

pub(crate) use entries::*;
use refusals::*;
pub(crate) use tasks::*;

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

#[cfg(test)]
#[path = "../timeline_read_tests.rs"]
mod read_tests;

#[cfg(test)]
mod tests;
