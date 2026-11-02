//! Import jobs, and previewing and applying them.

use super::*;

// ── Import jobs ────────────────────────────────────────────────────────────────

/// `POST /imports` — upload a CSV and get back a draft job with inferred columns.
///
/// The file arrives as a JSON string, not multipart (see
/// [`crate::models::CreateImportRequest::csv`]). The size ceiling is the store's, so
/// an oversized upload comes back as a 422 naming the limit rather than a truncated
/// parse.
pub(super) async fn create_import(
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
pub(super) struct ImportListQuery {
    #[serde(default)]
    pub(super) object_id: Option<String>,
    #[serde(default)]
    pub(super) limit: Option<usize>,
}

/// `GET /imports` — recent jobs, newest first.
pub(super) async fn list_imports(
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
    let imports = state
        .store
        .list_imports(object_id.as_deref(), limit)
        .await?;
    Ok(Json(json!({ "imports": imports })))
}

/// `GET /imports/:import_id`. The raw CSV is never serialized — see [`ImportJob`].
pub(super) async fn get_import(
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
pub(super) async fn delete_import(
    State(state): State<AppState>,
    Path(import_id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !state.store.delete_import(&import_id).await? {
        return Err(ApiError::not_found("import"));
    }
    Ok(Json(json!({ "ok": true })))
}

// ── Preview & apply ────────────────────────────────────────────────────────────

/// `POST /imports/:import_id/preview` — the dry run. Writes nothing to `records`.
pub(super) async fn preview_import(
    State(state): State<AppState>,
    Path(import_id): Path<String>,
) -> ApiResult<Json<ImportPreview>> {
    let job = load_job(&state, &import_id).await?;
    reject_if_applied(&job, ImportStep::Preview)?;
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
pub(super) async fn apply_import(
    State(state): State<AppState>,
    Path(import_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let job = load_job(&state, &import_id).await?;
    reject_if_applied(&job, ImportStep::Apply)?;
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
pub(super) async fn emit_import_events(
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
        tracing::warn!(
            object_id,
            "ryu-crm: import applied but its object is gone; no events"
        );
        return Ok(0);
    };

    let mut emitted = 0usize;
    for record_id in result.created_record_ids.iter().take(EVENT_FANOUT_LIMIT) {
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
