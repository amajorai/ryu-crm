//! Search, reports, and the agent-facing tool surface.
//!
//! Three groups of routes that share one property: **they all read through store
//! functions that already exist, and none of them owns any domain logic.** FTS
//! ranking lives in `store::search`, the stage maths in `store::pipeline_report` /
//! `store::funnel_report`, validation in `store::validate_field_value`. What this
//! module adds is the HTTP shape and — for `/tools/*` — the one-line English
//! sentence a model actually reads.
//!
//! Paths are MOUNT-LOCAL. `main` nests the merged router under `/api/crm`, so a
//! route declared here as `/search` serves `/api/crm/search` and (through Core's
//! ext-proxy) `/api/ext/@ryu/crm/search`. Writing the prefix here would produce
//! `/api/crm/api/crm/search`.
//!
//! Two conventions in here are deliberate and easy to "fix" wrongly:
//!
//! 1. **`/tools/*` answers 200 with `{"ok": false}` for every domain failure** — an
//!    unknown object, a rejected value bag, a missing record. Only an infrastructure
//!    fault becomes a non-2xx. The caller is a language model reading a transcript,
//!    and a 404 body it never sees teaches it nothing, whereas
//!    `"there is no object called \"custmer\""` is a correction it can act on. The
//!    panel-facing routes above keep the normal status codes.
//! 2. **Every paginated call clamps its own limit.** `Config::clamp_limit` is the
//!    single ceiling, and the `limit`/`offset` fields on `SearchQuery` /
//!    `RecordQuery` are wire-only — the store reads the positional arguments and
//!    ignores the struct fields. Forwarding `body.limit` compiles and has no ceiling.

use axum::{
    extract::{Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::events;
use crate::models::*;
use crate::state::AppState;

/// How many recent timeline entries `GET /summary` returns when the caller names no
/// count. Much smaller than `Config::default_page_size`, because this is the dock
/// panel's header strip, not a feed: fifty activities is four screens of scroll
/// nobody asked for on first paint.
const DEFAULT_RECENT_ACTIVITY: usize = 10;

/// Default page size for a `/tools/*` read.
///
/// Deliberately tiny compared with the panel's 50. Every row a tool returns is spent
/// out of the model's context window, and a tool that dumps fifty records to answer
/// "which company is Acme" has made the transcript worse, not better. A caller that
/// genuinely wants more passes `limit`, and `clamp_limit` still caps it.
const TOOL_LIMIT: usize = 10;

/// How many hit titles a tool's `summary` sentence names before it stops. The full
/// set is always in `data`; this is only the part that reads as a sentence.
const TOOL_SUMMARY_NAMES: usize = 3;

/// Stamped into `created_by` / `author` on anything a `/tools/*` route writes, so the
/// panel can tell an agent's row from a human's. A fixed string rather than the
/// agent's name because this process has no identity to read one from — Core's
/// ext-proxy authenticates the plugin, not the conversation.
const TOOL_ACTOR: &str = "agent";

/// Build this module's router. State is applied once by `main`, so every module
/// returns `Router<AppState>` rather than taking the state itself.
pub fn routes() -> Router<AppState> {
    Router::new()
        // ── Search & reports ──
        .route("/search", get(search))
        .route("/summary", get(summary))
        .route("/reports/pipeline", post(pipeline))
        .route("/reports/funnel", post(funnel))
        // The FTS repair hatch. POST, not GET: it rewrites the whole index.
        .route("/reindex", post(reindex))
        // ── Agent tools ──
        //
        // A separate `/tools/` namespace rather than pointing the manifest's tool
        // definitions at the CRUD routes above, for two reasons that are both about
        // blast radius: these take SLUGS (a model cannot hold `fld_deal_stage` in
        // its head) and they cover records and activities ONLY. Nothing here can
        // edit the schema, delete an object, or run an import — an agent that can
        // rewrite the schema is an agent that can destroy the CRM.
        .route("/tools/search", post(tool_search))
        .route("/tools/find_record", post(tool_find_record))
        .route("/tools/get_record", post(tool_get_record))
        .route("/tools/create_record", post(tool_create_record))
        .route("/tools/update_record", post(tool_update_record))
        .route("/tools/log_activity", post(tool_log_activity))
        .route("/tools/create_task", post(tool_create_task))
        .route("/tools/pipeline", post(tool_pipeline))
}

// ── Search ─────────────────────────────────────────────────────────────────────

/// `GET /search` query string.
///
/// A local struct rather than `Query<SearchQuery>` because `SearchQuery::object_ids`
/// is a `Vec<String>`, and `serde_urlencoded` — what axum's `Query` runs on — cannot
/// deserialize a sequence from repeated keys. It fails the whole request rather than
/// collecting them, so `?object_ids=a&object_ids=b` would 400. Comma-separated is the
/// encoding that actually survives the round trip.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct SearchParams {
    /// `?q=` is the primary spelling (it is what a search box sends); `?query=`
    /// matches the JSON field name so the two surfaces agree.
    #[serde(default, alias = "query")]
    q: String,
    /// Comma-separated object ids or slugs. `?object_id=deal` is accepted for the
    /// common single-object case.
    #[serde(default, alias = "object_id", alias = "object")]
    object_ids: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

/// Full-text search across every object's records.
//
// `SearchResponse::hits` is already ordered by bm25 (LOWER is better) — the store
// says so and this handler does not re-sort, so neither should a client.
#[utoipa::path(
    get,
    path = "/api/crm/search",
    tag = "CRM",
    summary = "full-text search across every CRM record — companies, people, deals and any custom object at once.",
    params(
        ("q" = String, Query, description = "What to search for. Free text, matched across every searchable field."),
        ("object_ids" = Option<String>, Query, description = "Comma-separated object slugs or ids to scope the search to, e.g. `company,person`. Omit to search everything."),
        ("limit" = Option<usize>, Query, description = "Max hits. Clamped to the node's ceiling."),
        ("offset" = Option<usize>, Query, description = "How many hits to skip, for paging.")
    ),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> ApiResult<Json<SearchResponse>> {
    let limit = state.config.clamp_limit(params.limit);
    let offset = params.offset.unwrap_or(0);
    let query = SearchQuery {
        query: params.q,
        object_ids: split_list(params.object_ids.as_deref()),
        // `limit`/`offset` here are wire-only; the store reads the arguments below.
        ..Default::default()
    };
    Ok(Json(state.store.search(&query, limit, offset).await?))
}

/// `GET /summary` query string.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct SummaryParams {
    /// How many recent activities to include. `?limit=` is accepted because that is
    /// what every other list route on this app calls it.
    #[serde(default, alias = "limit")]
    recent: Option<usize>,
}

/// The dock panel's header strip in one request: object counts, task counts, recent
/// activity, and the deal pipeline.
#[utoipa::path(
    get,
    path = "/api/crm/summary",
    tag = "CRM",
    summary = "one-shot CRM overview: per-object record counts, open and overdue task counts, recent activity, and the deal pipeline.",
    params(("recent" = Option<usize>, Query, description = "How many recent timeline entries to include. Defaults to 10.")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn summary(
    State(state): State<AppState>,
    Query(params): Query<SummaryParams>,
) -> ApiResult<Json<CrmSummary>> {
    let recent = state
        .config
        .clamp_limit(params.recent.or(Some(DEFAULT_RECENT_ACTIVITY)));
    Ok(Json(state.store.summary(recent).await?))
}

/// Rebuild the FTS index from the records table.
//
// The repair hatch, exposed so a support session can fix a search index without a
// shell: a database restored from a backup taken mid-write, or one whose schema
// changed what `FieldType::is_searchable` returns.
// No `params`/`request_body`: the handler takes only `State`, so the derived tool is
// a zero-argument call. That is correct — a reindex has nothing to configure.
#[utoipa::path(
    post,
    path = "/api/crm/reindex",
    tag = "CRM",
    summary = "rebuild the CRM full-text search index from the records table (a repair hatch for when search returns stale or missing hits).",
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn reindex(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let reindexed = state.store.reindex_all().await?;
    Ok(Json(json!({ "ok": true, "reindexed": reindexed })))
}

// ── Reports ────────────────────────────────────────────────────────────────────

/// Counts and summed currency per stage of a status field, plus win rate.
//
// `pipeline_report` returns `None` for two different reasons — no such object, and
// the object has no status field — so this handler resolves the object first and
// splits them. Collapsing both into one 404 sends whoever debugs it looking for a
// missing record when the real answer is "Companies has no status field".
#[utoipa::path(
    post,
    path = "/api/crm/reports/pipeline",
    tag = "CRM",
    summary = "record counts and summed value per pipeline stage, plus win rate, for one object.",
    request_body = PipelineRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn pipeline(
    State(state): State<AppState>,
    Json(body): Json<PipelineRequest>,
) -> ApiResult<Json<PipelineReport>> {
    let object = resolve_report_object(&state, body.object_id.as_deref()).await?;
    state
        .store
        .pipeline_report(&body)
        .await?
        .map(Json)
        .ok_or_else(|| no_status_field(&object))
}

/// Per-stage entered / advanced / conversion rate / average age in stage.
#[utoipa::path(
    post,
    path = "/api/crm/reports/funnel",
    tag = "CRM",
    summary = "per-stage conversion: how many records entered each stage in a window, how many advanced, and the average time spent there.",
    request_body = FunnelRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn funnel(
    State(state): State<AppState>,
    Json(body): Json<FunnelRequest>,
) -> ApiResult<Json<FunnelReport>> {
    let object = resolve_report_object(&state, body.object_id.as_deref()).await?;
    state
        .store
        .funnel_report(&body)
        .await?
        .map(Json)
        .ok_or_else(|| no_status_field(&object))
}

/// The object a report runs over: the named one, else `deal` — matching the default
/// both store report functions apply internally.
async fn resolve_report_object(state: &AppState, requested: Option<&str>) -> ApiResult<Object> {
    let object_ref = requested
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("deal");
    state
        .store
        .get_object(object_ref)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("object \"{object_ref}\"")))
}

/// 400, not 404: the object was found, it simply has nothing to bucket by. A caller
/// that gets this should offer to pick a different object, not retry the same one.
fn no_status_field(object: &Object) -> ApiError {
    ApiError::bad_request(format!(
        "\"{}\" has no status field to report on",
        object.plural
    ))
}

// ── Agent tools ────────────────────────────────────────────────────────────────
//
// Every handler below returns `ApiResult<Json<ToolResponse>>` and reaches for
// `ToolResponse::failed` — never an `ApiError` — whenever the failure is something
// the caller could have written differently. `?` is still used on store calls, so a
// genuine SQL fault is still a 500.

/// Search records and hand back the top hits as a sentence plus the raw response.
#[utoipa::path(
    post,
    path = "/api/crm/tools/search",
    tag = "CRM",
    summary = "search the CRM and get the top hits back as one readable sentence plus the raw rows.",
    request_body = ToolSearchRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn tool_search(
    State(state): State<AppState>,
    Json(body): Json<ToolSearchRequest>,
) -> ApiResult<Json<ToolResponse>> {
    let needle = body.query.trim().to_string();
    if needle.is_empty() {
        return Ok(Json(ToolResponse::failed("a search needs a query")));
    }
    let limit = state.config.clamp_limit(body.limit.or(Some(TOOL_LIMIT)));
    let query = SearchQuery {
        query: needle.clone(),
        object_ids: body
            .object
            .as_deref()
            .map(str::trim)
            .filter(|o| !o.is_empty())
            .map(|o| vec![o.to_string()])
            .unwrap_or_default(),
        ..Default::default()
    };
    let response = state.store.search(&query, limit, 0).await?;
    let summary = if response.hits.is_empty() {
        format!("No records match \"{needle}\".")
    } else {
        format!(
            "Found {} matching \"{needle}\"{}.",
            count_of(response.total, "record"),
            name_list(response.hits.iter().map(|h| h.title.as_str())),
        )
    };
    Ok(Json(ToolResponse::ok(
        summary,
        serde_json::to_value(&response)?,
    )))
}

/// Look one record up by a field value — the "does Acme already exist" call an agent
/// makes before creating a duplicate.
///
/// Two pieces of resolution happen here that a naive equality filter would get wrong,
/// and both are why this is not just `POST /objects/:object/records/query` with a
/// different name:
///
/// * **A `select`/`status` value is stored as an OPTION ID**, so filtering on the
///   literal `"Qualified"` matches nothing. The label is resolved through the field's
///   own config first.
/// * **Exact match, then containment.** An agent that has "Acme" will not find "Acme
///   Corporation" with `eq`, and an agent that has an email address must not have it
///   loosened into a substring scan. So the fallback runs only for textual fields,
///   and only after the exact pass came back empty — which keeps a unique-field
///   lookup exact.
#[utoipa::path(
    post,
    path = "/api/crm/tools/find_record",
    tag = "CRM",
    summary = "look a record up by an exact field value — the check to run before creating anything, so an existing company is updated instead of duplicated.",
    request_body = ToolFindRecordRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn tool_find_record(
    State(state): State<AppState>,
    Json(body): Json<ToolFindRecordRequest>,
) -> ApiResult<Json<ToolResponse>> {
    let Some(object) = state.store.get_object(body.object.trim()).await? else {
        return Ok(Json(ToolResponse::failed(format!(
            "there is no object called \"{}\"",
            body.object.trim()
        ))));
    };
    let field_ref = match body
        .field
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        Some(field) => field.to_string(),
        None => match object.title_field_id.clone() {
            Some(id) => id,
            None => {
                return Ok(Json(ToolResponse::failed(format!(
                    "\"{}\" has no title field, so a field to match on must be named",
                    object.plural
                ))))
            }
        },
    };
    let Some(field) = state.store.resolve_field(&object.id, &field_ref).await? else {
        return Ok(Json(ToolResponse::failed(format!(
            "\"{}\" has no field called \"{field_ref}\"",
            object.plural
        ))));
    };

    let raw = body.value.trim();
    // An option-backed field stores the option id, so match the LABEL through the
    // field config rather than against the stored value.
    let needle = if field.field_type.is_option_backed() {
        match field.config.resolve_option(raw) {
            Some(option) => option.id.clone(),
            None => {
                return Ok(Json(ToolResponse::failed(format!(
                    "\"{raw}\" is not one of {}'s options",
                    field.name
                ))))
            }
        }
    } else {
        raw.to_string()
    };

    let limit = state.config.clamp_limit(body.limit.or(Some(TOOL_LIMIT)));
    let mut page =
        find_by_field(&state, &object, &field, FilterOperator::Eq, &needle, limit).await?;
    let mut matched_exactly = true;
    if page.items.is_empty() && is_textual(field.field_type) {
        page = find_by_field(
            &state,
            &object,
            &field,
            FilterOperator::Contains,
            &needle,
            limit,
        )
        .await?;
        matched_exactly = false;
    }

    let summary = if page.items.is_empty() {
        format!(
            "No {} has {} \"{raw}\".",
            object.plural.to_lowercase(),
            field.name.to_lowercase()
        )
    } else {
        format!(
            "Found {} where {} {} \"{raw}\"{}.",
            count_of(page.total, &object.singular.to_lowercase()),
            field.name.to_lowercase(),
            if matched_exactly { "is" } else { "contains" },
            name_list(page.items.iter().map(|r| r.title.as_str())),
        )
    };
    Ok(Json(ToolResponse::ok(
        summary,
        json!({
            "object_id": object.id,
            "field_id": field.id,
            "matched_exactly": matched_exactly,
            "records": page.items,
            "total": page.total,
        }),
    )))
}

/// One equality/containment query against one field. Split out only because
/// `tool_find_record` runs it twice with different operators.
async fn find_by_field(
    state: &AppState,
    object: &Object,
    field: &Field,
    op: FilterOperator,
    needle: &str,
    limit: usize,
) -> ApiResult<RecordPage> {
    let query = RecordQuery {
        object_id: object.id.clone(),
        filter: Some(ViewFilter::Condition(FilterCondition {
            field_id: field.id.clone(),
            op,
            value: json!(needle),
        })),
        ..Default::default()
    };
    Ok(state.store.query_records(&query, limit, 0).await?)
}

/// `POST /tools/get_record` body.
///
/// A local struct: `models` has no `ToolGetRecordRequest` because the shape is a bare
/// id and nothing else round-trips it.
// `pub(crate)` only so `api::mod`'s `components(schemas(...))` list can name it; the
// router still keeps it out of every other module's reach.
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub(crate) struct ToolGetRecordRequest {
    /// The record's id, as returned by search / find_record / create_record.
    #[serde(alias = "id")]
    pub record_id: String,
}

/// Read one record back in full — values, schema, links, and the recent timeline.
///
/// The read half of the create/update pair. Without it an agent that just wrote a
/// record has no way to see what the store normalized its values into (`"$1,234.56"`
/// became `123456` cents; `"Qualified"` became `opt_deal_stage_qualified`).
#[utoipa::path(
    post,
    path = "/api/crm/tools/get_record",
    tag = "CRM",
    summary = "read one record in full — every field value, its schema, its linked records, and its recent timeline.",
    request_body = ToolGetRecordRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn tool_get_record(
    State(state): State<AppState>,
    Json(body): Json<ToolGetRecordRequest>,
) -> ApiResult<Json<ToolResponse>> {
    let record_id = body.record_id.trim();
    let Some(detail) = state.store.get_record_detail(record_id).await? else {
        return Ok(Json(ToolResponse::failed(format!(
            "there is no record with id \"{record_id}\""
        ))));
    };
    let summary = format!(
        "{} “{}” ({}) — {}, {}, {}.",
        detail.object.singular,
        detail.record.title,
        detail.record.id,
        count_of(detail.record.values.len() as i64, "value"),
        count_of(detail.links.len() as i64, "linked record"),
        count_of(detail.activities.len() as i64, "timeline entry"),
    );
    Ok(Json(ToolResponse::ok(
        summary,
        serde_json::to_value(&detail)?,
    )))
}

/// Create a record from slug-keyed values.
///
/// The values go through the store's validator untouched, which is what makes the
/// tool tolerant of what a model actually writes: `"Qualified"` resolves to its
/// option id, `"$1,234.56"` and `"31 Mar 2026"` normalize exactly as a form save
/// would. A rejection comes back as `ok: false` with every field reason, so the model
/// can fix all of them in one retry instead of discovering them one at a time.
#[utoipa::path(
    post,
    path = "/api/crm/tools/create_record",
    tag = "CRM",
    summary = "create a CRM record (company, person, deal, or a custom object) from slug-keyed values.",
    request_body = ToolCreateRecordRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn tool_create_record(
    State(state): State<AppState>,
    Json(body): Json<ToolCreateRecordRequest>,
) -> ApiResult<Json<ToolResponse>> {
    let Some(object) = state.store.get_object(body.object.trim()).await? else {
        return Ok(Json(ToolResponse::failed(format!(
            "there is no object called \"{}\"",
            body.object.trim()
        ))));
    };
    let request = CreateRecordRequest {
        values: body.values.clone(),
        created_by: Some(TOOL_ACTOR.to_string()),
    };
    let record = match state.store.create_record(&object.id, &request).await? {
        Ok(record) => record,
        Err(errors) => return Ok(Json(ToolResponse::failed(describe_validation(&errors)))),
    };
    // After the commit, never before — a consumer that reacts by reading the record
    // back must not lose the race.
    events::record_created(&state.events, &record, &object).await;
    let summary = format!(
        "Created {} “{}” ({}).",
        object.singular.to_lowercase(),
        record.title,
        record.id
    );
    Ok(Json(ToolResponse::ok(
        summary,
        serde_json::to_value(&record)?,
    )))
}

/// Merge slug-keyed values into an existing record.
///
/// Merge, never replace: a model sends the two fields it learned about, and a replace
/// would silently blank everything it did not mention.
#[utoipa::path(
    post,
    path = "/api/crm/tools/update_record",
    tag = "CRM",
    summary = "merge new values into an existing record; fields left out keep their current values.",
    request_body = ToolUpdateRecordRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn tool_update_record(
    State(state): State<AppState>,
    Json(body): Json<ToolUpdateRecordRequest>,
) -> ApiResult<Json<ToolResponse>> {
    let record_id = body.record_id.trim();
    let request = UpdateRecordRequest {
        values: body.values.clone(),
        mode: UpdateMode::Merge,
    };
    let update = match state.store.update_record(record_id, &request).await? {
        Ok(Some(update)) => update,
        Ok(None) => {
            return Ok(Json(ToolResponse::failed(format!(
                "there is no record with id \"{record_id}\""
            ))))
        }
        Err(errors) => return Ok(Json(ToolResponse::failed(describe_validation(&errors)))),
    };

    emit_record_update(&state, &update).await?;

    let summary = if update.changed.is_empty() {
        // An honest no-op. Reporting "updated" here would teach the model that its
        // write landed when nothing moved, and it would stop retrying a real failure.
        format!(
            "“{}” was already up to date; nothing changed.",
            update.record.title
        )
    } else {
        format!(
            "Updated “{}” ({}): {}.",
            update.record.title,
            update.record.id,
            update
                .changed
                .iter()
                .map(|c| c.field_name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Ok(Json(ToolResponse::ok(
        summary,
        serde_json::to_value(&update)?,
    )))
}

/// Raise the events a record update owes, if any.
///
/// Kept here rather than inlined twice because the "only when `changed` is non-empty"
/// rule is the sort of condition that gets dropped on the second copy. A stage move
/// always implies a change, so the stage emit nests inside that guard rather than
/// standing beside it.
async fn emit_record_update(state: &AppState, update: &RecordUpdate) -> ApiResult<()> {
    if update.changed.is_empty() {
        return Ok(());
    }
    let Some(object) = state.store.get_object(&update.record.object_id).await? else {
        return Ok(());
    };
    events::record_updated(&state.events, &update.record, &object, &update.changed).await;
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
    Ok(())
}

/// Append a note / call / meeting / task to a record's timeline.
///
/// The record id is NOT pre-checked: `create_activity` already rejects an unknown one
/// with a `record_id` field error, and a second lookup would only duplicate it — one
/// more lock for the same sentence.
#[utoipa::path(
    post,
    path = "/api/crm/tools/log_activity",
    tag = "CRM",
    summary = "append a note, call, or meeting to a record's timeline.",
    request_body = ToolLogActivityRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn tool_log_activity(
    State(state): State<AppState>,
    Json(body): Json<ToolLogActivityRequest>,
) -> ApiResult<Json<ToolResponse>> {
    let request = CreateActivityRequest {
        record_id: Some(body.record_id.trim().to_string()),
        kind: body.kind,
        title: activity_title(&body.title, body.body.as_deref(), body.kind),
        body: body.body.clone(),
        assignee: body.assignee.clone(),
        due_at: body.due_at.clone(),
        author: Some(TOOL_ACTOR.to_string()),
        metadata: None,
    };
    let activity = match state.store.create_activity(&request).await? {
        Ok(activity) => activity,
        Err(errors) => return Ok(Json(ToolResponse::failed(describe_validation(&errors)))),
    };
    let on = describe_record(&state, activity.record_id.as_deref()).await?;
    let summary = format!(
        "Logged {} on {on}: “{}”.",
        article_for(activity.kind),
        activity.title
    );
    Ok(Json(ToolResponse::ok(
        summary,
        serde_json::to_value(&activity)?,
    )))
}

/// `POST /tools/create_task` body.
///
/// A local struct, and a separate route from `log_activity`, for one structural
/// reason: `ToolLogActivityRequest::record_id` is NOT optional, so a standalone task —
/// "remind me to send the deck on Friday", attached to no record — cannot be
/// expressed through that shape at all.
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub(crate) struct ToolCreateTaskRequest {
    /// The record the task hangs off. Omit for a standalone task that belongs to no
    /// record.
    #[serde(default)]
    pub record_id: Option<String>,
    /// One-line description of what needs doing. Left empty, the first line of `body`
    /// is used.
    #[serde(default)]
    pub title: String,
    /// Longer detail, if the title is not enough.
    #[serde(default)]
    pub body: Option<String>,
    /// Who owns the task. Free text.
    #[serde(default)]
    pub assignee: Option<String>,
    /// When it is due, RFC-3339 or any common date form. A task with no due date is
    /// valid — it simply never becomes overdue.
    #[serde(default)]
    pub due_at: Option<String>,
}

/// Create a task, optionally hung off a record.
#[utoipa::path(
    post,
    path = "/api/crm/tools/create_task",
    tag = "CRM",
    summary = "create a follow-up task, optionally attached to a record and optionally with a due date.",
    request_body = ToolCreateTaskRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn tool_create_task(
    State(state): State<AppState>,
    Json(body): Json<ToolCreateTaskRequest>,
) -> ApiResult<Json<ToolResponse>> {
    let request = CreateActivityRequest {
        record_id: body
            .record_id
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_string),
        kind: ActivityKind::Task,
        title: activity_title(&body.title, body.body.as_deref(), ActivityKind::Task),
        body: body.body.clone(),
        assignee: body.assignee.clone(),
        due_at: body.due_at.clone(),
        author: Some(TOOL_ACTOR.to_string()),
        metadata: None,
    };
    let activity = match state.store.create_activity(&request).await? {
        Ok(activity) => activity,
        Err(errors) => return Ok(Json(ToolResponse::failed(describe_validation(&errors)))),
    };
    let due = match activity.due_at.as_deref() {
        Some(due) => format!(", due {due}"),
        None => String::new(),
    };
    let on = match activity.record_id.as_deref() {
        Some(_) => format!(
            " on {}",
            describe_record(&state, activity.record_id.as_deref()).await?
        ),
        None => String::new(),
    };
    Ok(Json(ToolResponse::ok(
        format!("Created task “{}”{on}{due}.", activity.title),
        serde_json::to_value(&activity)?,
    )))
}

/// The pipeline report with an English headline in front of it.
///
/// The same `store::pipeline_report` the panel's chart calls — the only thing this
/// adds is `summary`, because a model handed a `PipelineReport` struct will otherwise
/// paraphrase the numbers itself and get the currency scale wrong.
#[utoipa::path(
    post,
    path = "/api/crm/tools/pipeline",
    tag = "CRM",
    summary = "the deal pipeline as an English headline plus the numbers — totals per stage, win rate, and anything sitting outside a stage.",
    request_body = PipelineRequest,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub(crate) async fn tool_pipeline(
    State(state): State<AppState>,
    Json(body): Json<PipelineRequest>,
) -> ApiResult<Json<ToolResponse>> {
    let object_ref = body
        .object_id
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("deal");
    let Some(object) = state.store.get_object(object_ref).await? else {
        return Ok(Json(ToolResponse::failed(format!(
            "there is no object called \"{object_ref}\""
        ))));
    };
    let Some(report) = state.store.pipeline_report(&body).await? else {
        return Ok(Json(ToolResponse::failed(format!(
            "\"{}\" has no status field to report on",
            object.plural
        ))));
    };
    Ok(Json(ToolResponse::ok(
        pipeline_sentence(&object, &report),
        serde_json::to_value(&report)?,
    )))
}

/// One or two sentences describing a pipeline report.
///
/// `unassigned_count` is named whenever it is non-zero, for the same reason the store
/// counts it separately instead of dropping it: a forecast that quietly excludes rows
/// is how a number ends up wrong in a board meeting.
fn pipeline_sentence(object: &Object, report: &PipelineReport) -> String {
    let mut out = format!(
        "{}: {} worth {} across {}.",
        object.plural,
        count_of(report.total_records, "record"),
        format_cents(report.total_value_cents, &report.currency_code),
        count_of(report.stages.len() as i64, "stage"),
    );
    if report.unassigned_count > 0 {
        out.push_str(&format!(
            " {} sitting outside every stage.",
            count_of(report.unassigned_count, "record"),
        ));
    }
    if report.won_count > 0 || report.lost_count > 0 {
        out.push_str(&format!(
            " Won {} ({}), lost {} — {:.0}% win rate.",
            report.won_count,
            format_cents(report.won_value_cents, &report.currency_code),
            report.lost_count,
            report.win_rate * 100.0,
        ));
    }
    let breakdown = report
        .stages
        .iter()
        .filter(|s| s.record_count > 0)
        .map(|s| format!("{} {}", s.record_count, s.label))
        .collect::<Vec<_>>();
    if !breakdown.is_empty() {
        out.push_str(&format!(" By stage: {}.", breakdown.join(", ")));
    }
    out
}

// ── Shared helpers ─────────────────────────────────────────────────────────────

/// Split a comma-separated query-string list into trimmed, non-empty parts.
fn split_list(raw: Option<&str>) -> Vec<String> {
    raw.map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

/// `"3 records"` / `"1 record"`. Naive `-s` pluralisation, which is correct for every
/// noun this file passes it ("record", "stage", "deal", "timeline entry" → "entries"
/// is handled by the caller passing an already-plural-safe noun).
fn count_of(n: i64, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else if noun.ends_with('y') && !noun.ends_with("ay") && !noun.ends_with("ey") {
        format!("{n} {}ies", &noun[..noun.len() - 1])
    } else {
        format!("{n} {noun}s")
    }
}

/// `": Acme, Initech and 4 more"`, or an empty string when there is nothing to name.
/// Appended to a count, so it never repeats the total.
fn name_list<'a>(titles: impl Iterator<Item = &'a str>) -> String {
    let titles: Vec<&str> = titles.collect();
    if titles.is_empty() {
        return String::new();
    }
    let shown = titles
        .iter()
        .take(TOOL_SUMMARY_NAMES)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if titles.len() > TOOL_SUMMARY_NAMES {
        format!(": {shown} and {} more", titles.len() - TOOL_SUMMARY_NAMES)
    } else {
        format!(": {shown}")
    }
}

/// Integer cents → `"1,234.56 USD"`.
///
/// Money is cents everywhere in this app, and this is the ONE place the scale is
/// undone. Printing `report.total_value_cents` raw into a sentence a model then
/// repeats is a 100× error that reads perfectly plausibly.
fn format_cents(cents: i64, currency: &str) -> String {
    let negative = cents < 0;
    let magnitude = cents.unsigned_abs();
    let major = magnitude / 100;
    let minor = magnitude % 100;
    let mut grouped = String::new();
    let digits = major.to_string();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let sign = if negative { "-" } else { "" };
    format!("{sign}{grouped}.{minor:02} {currency}")
}

/// Flatten a validation rejection into one line a model can act on.
///
/// Every reason, not just the first: the store collects rather than short-circuits
/// precisely so a caller with four bad fields fixes four fields in one retry.
fn describe_validation(errors: &[FieldValidationError]) -> String {
    if errors.is_empty() {
        return "the values were rejected".to_string();
    }
    errors
        .iter()
        .map(|e| format!("{}: {}", e.field_slug, e.message))
        .collect::<Vec<_>>()
        .join("; ")
}

/// A timeline entry needs a readable title. A model that fills in only `body` would
/// otherwise leave a blank row in the record drawer — the store accepts an empty
/// title, so this is the wrapper's job, not the store's.
fn activity_title(title: &str, body: Option<&str>, kind: ActivityKind) -> String {
    let trimmed = title.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    let from_body = body
        .and_then(|b| b.lines().find(|line| !line.trim().is_empty()))
        .map(str::trim)
        .unwrap_or("");
    if from_body.is_empty() {
        return format!("Untitled {}", kind.as_str().replace('_', " "));
    }
    // One line, and short enough to sit in a timeline row without wrapping.
    from_body.chars().take(80).collect()
}

/// `"the deal “Acme renewal” (rec_…)"`, or the bare id when it cannot be resolved.
/// Only reached on a success path, so the lookup costs nothing when a write failed.
async fn describe_record(state: &AppState, record_id: Option<&str>) -> ApiResult<String> {
    let Some(record_id) = record_id else {
        return Ok("no record".to_string());
    };
    match state.store.get_record(record_id).await? {
        Some(record) => Ok(format!("“{}” ({})", record.title, record.id)),
        None => Ok(record_id.to_string()),
    }
}

/// `"a note"` / `"a call"` / `"a meeting"` / `"a task"`.
fn article_for(kind: ActivityKind) -> String {
    let word = kind.as_str().replace('_', " ");
    let article = if word.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    format!("{article} {word}")
}

/// Whether a `contains` fallback makes sense for this field type.
///
/// Deliberately an allow-list. A `contains` scan over a number, a date or an option
/// id is a substring match on a serialized value — it "works" and returns garbage,
/// which is worse than returning nothing.
fn is_textual(field_type: FieldType) -> bool {
    matches!(
        field_type,
        FieldType::Text
            | FieldType::LongText
            | FieldType::Email
            | FieldType::Url
            | FieldType::Phone
            | FieldType::User
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bag(pairs: &[(&str, Value)]) -> ValueBag {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    async fn state() -> AppState {
        AppState::in_memory().expect("in-memory state")
    }

    /// Seed one record straight through the store, so the fixture goes through the
    /// same validation and FTS maintenance a real write does.
    async fn seed(state: &AppState, object: &str, values: &[(&str, Value)]) -> Record {
        state
            .store
            .create_record(
                object,
                &CreateRecordRequest {
                    values: bag(values),
                    created_by: None,
                },
            )
            .await
            .expect("no infrastructure failure")
            .expect("values accepted")
    }

    // ── Pure helpers ──

    #[test]
    fn comma_lists_survive_the_query_string_round_trip() {
        assert_eq!(split_list(None), Vec::<String>::new());
        assert_eq!(split_list(Some("")), Vec::<String>::new());
        assert_eq!(split_list(Some("deal")), vec!["deal".to_string()]);
        assert_eq!(
            split_list(Some(" deal , company ,, person ")),
            vec![
                "deal".to_string(),
                "company".to_string(),
                "person".to_string()
            ]
        );
    }

    #[test]
    fn cents_are_printed_as_money_not_as_cents() {
        // The 100× error this function exists to prevent: 123456 is $1,234.56.
        assert_eq!(format_cents(123_456, "USD"), "1,234.56 USD");
        assert_eq!(format_cents(0, "USD"), "0.00 USD");
        assert_eq!(format_cents(5, "USD"), "0.05 USD");
        assert_eq!(format_cents(99, "EUR"), "0.99 EUR");
        assert_eq!(format_cents(100, "USD"), "1.00 USD");
        assert_eq!(format_cents(1_000_00, "USD"), "1,000.00 USD");
        assert_eq!(format_cents(1_234_567_89, "USD"), "1,234,567.89 USD");
        assert_eq!(format_cents(-25_000, "USD"), "-250.00 USD");
    }

    #[test]
    fn counts_agree_with_their_noun() {
        assert_eq!(count_of(0, "record"), "0 records");
        assert_eq!(count_of(1, "record"), "1 record");
        assert_eq!(count_of(2, "record"), "2 records");
        assert_eq!(count_of(3, "timeline entry"), "3 timeline entries");
        assert_eq!(count_of(1, "timeline entry"), "1 timeline entry");
    }

    #[test]
    fn a_name_list_names_a_few_and_counts_the_rest() {
        assert_eq!(name_list(Vec::<&str>::new().into_iter()), "");
        assert_eq!(name_list(["Acme"].into_iter()), ": Acme");
        assert_eq!(
            name_list(["Acme", "Initech", "Hooli", "Umbrella", "Globex"].into_iter()),
            ": Acme, Initech, Hooli and 2 more"
        );
    }

    #[test]
    fn every_validation_reason_reaches_the_caller_not_just_the_first() {
        let message = describe_validation(&[
            FieldValidationError::new("fld_deal_name", "name", "this field is required"),
            FieldValidationError::new("fld_deal_stage", "stage", "this field is required"),
        ]);
        assert!(message.contains("name: this field is required"));
        assert!(message.contains("stage: this field is required"));
        assert_eq!(describe_validation(&[]), "the values were rejected");
    }

    #[test]
    fn a_titleless_activity_still_gets_a_readable_row() {
        assert_eq!(
            activity_title("  Called Ana ", None, ActivityKind::Call),
            "Called Ana"
        );
        assert_eq!(
            activity_title(
                "",
                Some("\n  Left a voicemail about renewal\nmore"),
                ActivityKind::Call
            ),
            "Left a voicemail about renewal"
        );
        assert_eq!(
            activity_title("", None, ActivityKind::Task),
            "Untitled task"
        );
        // Long bodies are clipped rather than wrapped into the timeline row.
        let long = "x".repeat(200);
        assert_eq!(
            activity_title("", Some(&long), ActivityKind::Note)
                .chars()
                .count(),
            80
        );
    }

    #[test]
    fn contains_fallback_is_an_allow_list_not_a_deny_list() {
        assert!(is_textual(FieldType::Text));
        assert!(is_textual(FieldType::Email));
        // A substring scan over these returns plausible garbage, so it must not run.
        assert!(!is_textual(FieldType::Currency));
        assert!(!is_textual(FieldType::Date));
        assert!(!is_textual(FieldType::Status));
        assert!(!is_textual(FieldType::Relation));
        assert!(!is_textual(FieldType::Checkbox));
    }

    // ── Search & reports ──

    #[tokio::test]
    async fn search_clamps_a_hostile_limit_to_the_configured_ceiling() {
        let state = state().await;
        seed(&state, "company", &[("name", json!("Acme Corporation"))]).await;
        let response = search(
            State(state.clone()),
            Query(SearchParams {
                q: "Acme".to_string(),
                limit: Some(100_000),
                ..Default::default()
            }),
        )
        .await
        .expect("search runs")
        .0;
        assert_eq!(response.limit, state.config.max_page_size);
        assert_eq!(response.hits.len(), 1);
        assert_eq!(response.hits[0].object_slug, "company");
        assert!(response.hits[0].snippet.contains("<mark>"));
    }

    #[tokio::test]
    async fn search_scopes_to_the_objects_named_in_the_query_string() {
        let state = state().await;
        seed(&state, "company", &[("name", json!("Acme Corporation"))]).await;
        seed(
            &state,
            "deal",
            &[("name", json!("Acme renewal")), ("stage", json!("Lead"))],
        )
        .await;

        let all = search(
            State(state.clone()),
            Query(SearchParams {
                q: "Acme".to_string(),
                ..Default::default()
            }),
        )
        .await
        .expect("search runs")
        .0;
        assert_eq!(all.total, 2);

        let scoped = search(
            State(state.clone()),
            Query(SearchParams {
                q: "Acme".to_string(),
                object_ids: Some(" deal ".to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("search runs")
        .0;
        assert_eq!(scoped.total, 1);
        assert_eq!(scoped.hits[0].object_slug, "deal");
    }

    #[tokio::test]
    async fn summary_defaults_to_a_header_strip_not_a_full_page() {
        let state = state().await;
        let response = summary(State(state.clone()), Query(SummaryParams::default()))
            .await
            .expect("summary runs")
            .0;
        assert_eq!(response.objects.len(), STANDARD_OBJECT_IDS.len());
        assert_eq!(response.total_records, 0);
        assert!(response.recent_activity.len() <= DEFAULT_RECENT_ACTIVITY);
        // The seeded deal object has a status field, so the pipeline is present.
        assert!(response.pipeline.is_some());
    }

    #[tokio::test]
    async fn a_report_on_a_missing_object_is_a_404_and_one_without_a_status_field_is_a_400() {
        let state = state().await;

        let missing = pipeline(
            State(state.clone()),
            Json(PipelineRequest {
                object_id: Some("nope".to_string()),
                include_closed: true,
                ..Default::default()
            }),
        )
        .await;
        assert!(matches!(missing, Err(ApiError::NotFound(_))));

        // `note` exists and has no status field — a different failure, and the split
        // is the whole reason this handler resolves the object itself.
        let no_status = pipeline(
            State(state.clone()),
            Json(PipelineRequest {
                object_id: Some("note".to_string()),
                include_closed: true,
                ..Default::default()
            }),
        )
        .await;
        assert!(matches!(no_status, Err(ApiError::BadRequest(_))));

        let funnel_missing = funnel(
            State(state.clone()),
            Json(FunnelRequest {
                object_id: Some("nope".to_string()),
                ..Default::default()
            }),
        )
        .await;
        assert!(matches!(funnel_missing, Err(ApiError::NotFound(_))));
    }

    #[tokio::test]
    async fn reindex_reports_how_many_records_it_rebuilt() {
        let state = state().await;
        seed(&state, "company", &[("name", json!("Acme Corporation"))]).await;
        seed(&state, "company", &[("name", json!("Initech"))]).await;
        let body = reindex(State(state.clone())).await.expect("reindex runs").0;
        assert_eq!(body["ok"], json!(true));
        assert_eq!(body["reindexed"], json!(2));
    }

    // ── Tools ──

    #[tokio::test]
    async fn a_status_label_is_resolved_to_its_option_id_before_matching() {
        let state = state().await;
        seed(
            &state,
            "deal",
            &[
                ("name", json!("Acme renewal")),
                ("stage", json!("Qualified")),
            ],
        )
        .await;

        // The record stores `opt_deal_stage_qualified`, so a literal equality filter
        // on "Qualified" would match nothing. The label must be resolved first.
        let found = tool_find_record(
            State(state.clone()),
            Json(ToolFindRecordRequest {
                object: "deal".to_string(),
                field: Some("stage".to_string()),
                value: "Qualified".to_string(),
                limit: None,
            }),
        )
        .await
        .expect("tool runs")
        .0;
        assert!(found.ok, "{}", found.summary);
        assert_eq!(found.data.as_ref().unwrap()["total"], json!(1));

        // An option that does not exist is a readable failure, not an empty result —
        // "no deal is in stage Frobnicated" would send the model looking for records.
        let bogus = tool_find_record(
            State(state.clone()),
            Json(ToolFindRecordRequest {
                object: "deal".to_string(),
                field: Some("stage".to_string()),
                value: "Frobnicated".to_string(),
                limit: None,
            }),
        )
        .await
        .expect("tool runs")
        .0;
        assert!(!bogus.ok);
        assert!(bogus.summary.contains("not one of"));
    }

    #[tokio::test]
    async fn find_record_falls_back_from_exact_to_containment_on_text_only() {
        let state = state().await;
        seed(&state, "company", &[("name", json!("Acme Corporation"))]).await;

        // Exact miss, containment hit — and the response says which happened, so a
        // caller can decide whether "close enough" is good enough.
        let loose = tool_find_record(
            State(state.clone()),
            Json(ToolFindRecordRequest {
                object: "company".to_string(),
                // No `field`: falls back to the object's title field.
                field: None,
                value: "Acme".to_string(),
                limit: None,
            }),
        )
        .await
        .expect("tool runs")
        .0;
        assert!(loose.ok, "{}", loose.summary);
        let data = loose.data.as_ref().unwrap();
        assert_eq!(data["total"], json!(1));
        assert_eq!(data["matched_exactly"], json!(false));
        assert!(loose.summary.contains("contains"));

        let exact = tool_find_record(
            State(state.clone()),
            Json(ToolFindRecordRequest {
                object: "company".to_string(),
                field: Some("name".to_string()),
                value: "Acme Corporation".to_string(),
                limit: None,
            }),
        )
        .await
        .expect("tool runs")
        .0;
        assert_eq!(exact.data.as_ref().unwrap()["matched_exactly"], json!(true));
    }

    #[tokio::test]
    async fn an_unknown_object_is_a_correctable_sentence_not_an_http_error() {
        let state = state().await;
        let response = tool_find_record(
            State(state.clone()),
            Json(ToolFindRecordRequest {
                object: "custmer".to_string(),
                field: None,
                value: "Acme".to_string(),
                limit: None,
            }),
        )
        .await
        .expect("the handler itself did not fail")
        .0;
        assert!(!response.ok);
        assert!(response.summary.contains("custmer"));
        assert!(response.data.is_none());
    }

    #[tokio::test]
    async fn tool_create_record_reports_every_rejected_field_at_once() {
        let state = state().await;
        // `name` and `stage` are both required on a deal.
        let rejected = tool_create_record(
            State(state.clone()),
            Json(ToolCreateRecordRequest {
                object: "deal".to_string(),
                values: bag(&[]),
            }),
        )
        .await
        .expect("the handler itself did not fail")
        .0;
        assert!(!rejected.ok);
        assert!(rejected.summary.contains("name"));
        assert!(rejected.summary.contains("stage"));
    }

    #[tokio::test]
    async fn tool_create_record_normalizes_through_the_same_validator_a_form_uses() {
        let state = state().await;
        let created = tool_create_record(
            State(state.clone()),
            Json(ToolCreateRecordRequest {
                object: "deal".to_string(),
                values: bag(&[
                    ("name", json!("Acme renewal")),
                    // A label, not an option id — what a model actually writes.
                    ("stage", json!("Qualified")),
                    // A STRING amount is a major-unit amount: $1,234.56 → 123456 cents.
                    ("amount", json!("$1,234.56")),
                ]),
            }),
        )
        .await
        .expect("tool runs")
        .0;
        assert!(created.ok, "{}", created.summary);
        let record: Record = serde_json::from_value(created.data.clone().unwrap()).unwrap();
        assert_eq!(record.values["stage"], json!("opt_deal_stage_qualified"));
        assert_eq!(record.values["amount"], json!(123_456));
        assert_eq!(record.created_by.as_deref(), Some(TOOL_ACTOR));
        assert!(created.summary.contains("Acme renewal"));
    }

    #[tokio::test]
    async fn tool_update_record_does_not_claim_a_change_it_did_not_make() {
        let state = state().await;
        let record = seed(
            &state,
            "deal",
            &[("name", json!("Acme renewal")), ("stage", json!("Lead"))],
        )
        .await;

        let moved = tool_update_record(
            State(state.clone()),
            Json(ToolUpdateRecordRequest {
                record_id: record.id.clone(),
                values: bag(&[("stage", json!("Won"))]),
            }),
        )
        .await
        .expect("tool runs")
        .0;
        assert!(moved.ok);
        assert!(moved.summary.starts_with("Updated"));
        let update: RecordUpdate = serde_json::from_value(moved.data.clone().unwrap()).unwrap();
        assert_eq!(update.changed.len(), 1);
        let stage = update.stage_change.expect("a status field moved");
        assert_eq!(stage.to.as_deref(), Some("opt_deal_stage_won"));

        // Writing the same value again must NOT read as a successful update, or a
        // model learns its write landed when nothing moved.
        let noop = tool_update_record(
            State(state.clone()),
            Json(ToolUpdateRecordRequest {
                record_id: record.id.clone(),
                values: bag(&[("stage", json!("Won"))]),
            }),
        )
        .await
        .expect("tool runs")
        .0;
        assert!(noop.summary.contains("already up to date"));

        let missing = tool_update_record(
            State(state.clone()),
            Json(ToolUpdateRecordRequest {
                record_id: "rec_nope".to_string(),
                values: bag(&[("name", json!("x"))]),
            }),
        )
        .await
        .expect("the handler itself did not fail")
        .0;
        assert!(!missing.ok);
    }

    #[tokio::test]
    async fn merge_semantics_mean_an_unmentioned_field_survives_a_tool_update() {
        let state = state().await;
        let record = seed(
            &state,
            "deal",
            &[
                ("name", json!("Acme renewal")),
                ("stage", json!("Lead")),
                ("amount", json!(250_000)),
            ],
        )
        .await;
        tool_update_record(
            State(state.clone()),
            Json(ToolUpdateRecordRequest {
                record_id: record.id.clone(),
                values: bag(&[("stage", json!("Proposal"))]),
            }),
        )
        .await
        .expect("tool runs");

        let after = state.store.get_record(&record.id).await.unwrap().unwrap();
        assert_eq!(after.values["amount"], json!(250_000));
        assert_eq!(after.values["name"], json!("Acme renewal"));
    }

    #[tokio::test]
    async fn tool_get_record_reads_back_what_the_store_normalized() {
        let state = state().await;
        let record = seed(&state, "company", &[("name", json!("Acme Corporation"))]).await;
        let response = tool_get_record(
            State(state.clone()),
            Json(ToolGetRecordRequest {
                record_id: record.id.clone(),
            }),
        )
        .await
        .expect("tool runs")
        .0;
        assert!(response.ok);
        assert!(response.summary.contains("Acme Corporation"));
        let detail: RecordDetail = serde_json::from_value(response.data.clone().unwrap()).unwrap();
        assert_eq!(detail.record.id, record.id);
        assert_eq!(detail.object.slug, "company");

        let missing = tool_get_record(
            State(state.clone()),
            Json(ToolGetRecordRequest {
                record_id: "rec_nope".to_string(),
            }),
        )
        .await
        .expect("the handler itself did not fail")
        .0;
        assert!(!missing.ok);
    }

    #[tokio::test]
    async fn log_activity_rejects_the_kinds_the_store_writes_itself() {
        let state = state().await;
        let record = seed(&state, "company", &[("name", json!("Acme Corporation"))]).await;

        let logged = tool_log_activity(
            State(state.clone()),
            Json(ToolLogActivityRequest {
                record_id: record.id.clone(),
                kind: ActivityKind::Call,
                title: String::new(),
                body: Some("Left a voicemail about the renewal".to_string()),
                assignee: None,
                due_at: None,
            }),
        )
        .await
        .expect("tool runs")
        .0;
        assert!(logged.ok, "{}", logged.summary);
        assert!(logged.summary.contains("Acme Corporation"));
        let activity: Activity = serde_json::from_value(logged.data.clone().unwrap()).unwrap();
        // The blank title was filled from the body rather than left empty.
        assert_eq!(activity.title, "Left a voicemail about the renewal");
        assert_eq!(activity.author.as_deref(), Some(TOOL_ACTOR));

        // A hand-forged audit entry is worse than none, so the store refuses and this
        // surfaces the refusal as a sentence.
        let forged = tool_log_activity(
            State(state.clone()),
            Json(ToolLogActivityRequest {
                record_id: record.id.clone(),
                kind: ActivityKind::StageChange,
                title: "faked".to_string(),
                body: None,
                assignee: None,
                due_at: None,
            }),
        )
        .await
        .expect("the handler itself did not fail")
        .0;
        assert!(!forged.ok);

        let orphan = tool_log_activity(
            State(state.clone()),
            Json(ToolLogActivityRequest {
                record_id: "rec_nope".to_string(),
                kind: ActivityKind::Note,
                title: "hello".to_string(),
                body: None,
                assignee: None,
                due_at: None,
            }),
        )
        .await
        .expect("the handler itself did not fail")
        .0;
        assert!(!orphan.ok);
    }

    #[tokio::test]
    async fn a_task_can_be_created_with_no_record_behind_it() {
        let state = state().await;
        let standalone = tool_create_task(
            State(state.clone()),
            Json(ToolCreateTaskRequest {
                record_id: None,
                title: "Send the deck".to_string(),
                due_at: Some("2026-09-01T09:00:00Z".to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("tool runs")
        .0;
        assert!(standalone.ok, "{}", standalone.summary);
        let activity: Activity = serde_json::from_value(standalone.data.clone().unwrap()).unwrap();
        assert_eq!(activity.kind, ActivityKind::Task);
        assert!(activity.record_id.is_none());
        assert!(activity.due_at.is_some());
        assert!(standalone.summary.contains("Send the deck"));

        // An empty-string record_id is treated as absent, not as a missing record —
        // a model that fills every key in a schema will send one.
        let blank = tool_create_task(
            State(state.clone()),
            Json(ToolCreateTaskRequest {
                record_id: Some("   ".to_string()),
                title: "Also standalone".to_string(),
                ..Default::default()
            }),
        )
        .await
        .expect("tool runs")
        .0;
        assert!(blank.ok, "{}", blank.summary);

        let bad_date = tool_create_task(
            State(state.clone()),
            Json(ToolCreateTaskRequest {
                record_id: None,
                title: "Whenever".to_string(),
                due_at: Some("next tuesday-ish".to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("the handler itself did not fail")
        .0;
        assert!(!bad_date.ok);
        assert!(bad_date.summary.contains("due_at"));
    }

    #[tokio::test]
    async fn the_pipeline_sentence_prints_money_as_money_and_names_the_unassigned() {
        let state = state().await;
        for (name, stage, amount) in [
            ("Acme renewal", "Won", 1_234_56_i64),
            ("Initech expansion", "Lost", 50_000),
            ("Hooli pilot", "Proposal", 250_000),
        ] {
            seed(
                &state,
                "deal",
                &[
                    ("name", json!(name)),
                    ("stage", json!(stage)),
                    ("amount", json!(amount)),
                ],
            )
            .await;
        }

        let response = tool_pipeline(
            State(state.clone()),
            Json(PipelineRequest {
                include_closed: true,
                ..Default::default()
            }),
        )
        .await
        .expect("tool runs")
        .0;
        assert!(response.ok, "{}", response.summary);
        let report: PipelineReport =
            serde_json::from_value(response.data.clone().unwrap()).unwrap();
        assert_eq!(report.total_records, 3);
        assert_eq!(report.won_count, 1);
        assert_eq!(report.lost_count, 1);
        // 1 won of 2 closed.
        assert!((report.win_rate - 0.5).abs() < f64::EPSILON);

        // The sentence must carry dollars, never the raw cents count.
        assert!(
            response.summary.contains("1,234.56 USD"),
            "won value not printed as money: {}",
            response.summary
        );
        assert!(
            response.summary.contains("50% win rate"),
            "{}",
            response.summary
        );
        assert!(response.summary.contains("By stage:"));

        let unknown = tool_pipeline(
            State(state.clone()),
            Json(PipelineRequest {
                object_id: Some("note".to_string()),
                include_closed: true,
                ..Default::default()
            }),
        )
        .await
        .expect("the handler itself did not fail")
        .0;
        assert!(!unknown.ok);
        assert!(unknown.summary.contains("no status field"));
    }

    #[tokio::test]
    async fn an_unassigned_record_is_named_rather_than_dropped_from_the_forecast() {
        let state = state().await;
        // `stage` is required on a deal, so the unassigned case is built on `company`,
        // whose `status` is optional.
        seed(&state, "company", &[("name", json!("Acme Corporation"))]).await;
        seed(
            &state,
            "company",
            &[("name", json!("Initech")), ("status", json!("Customer"))],
        )
        .await;

        let response = tool_pipeline(
            State(state.clone()),
            Json(PipelineRequest {
                object_id: Some("company".to_string()),
                include_closed: true,
                ..Default::default()
            }),
        )
        .await
        .expect("tool runs")
        .0;
        let report: PipelineReport =
            serde_json::from_value(response.data.clone().unwrap()).unwrap();
        assert_eq!(report.unassigned_count, 1);
        assert!(
            response.summary.contains("outside every stage"),
            "{}",
            response.summary
        );
    }
}
