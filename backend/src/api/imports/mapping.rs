//! The column-to-field mapping of an import job.

use super::*;

// ── Mapping ────────────────────────────────────────────────────────────────────

/// `GET /imports/:import_id/mapping` — everything the mapping screen needs in one
/// call: the parsed columns (with samples, the store's field suggestion and this
/// module's type guess), the mapping saved so far, the dedupe rule, and the object's
/// full field list to pick from.
///
/// One call rather than "fetch the job, then fetch the schema": the two are read
/// together every single time, and splitting them is a guaranteed extra round-trip on
/// a screen the user is already waiting on.
pub(super) async fn get_mapping(
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
pub(super) struct MappingEntryBody {
    pub(super) column_index: usize,
    /// An existing field's id or slug. `None` + no `create_field` ⇒ ignore the column.
    #[serde(default)]
    pub(super) field_id: Option<String>,
    /// Create a field and bind this column to it.
    #[serde(default)]
    pub(super) create_field: Option<NewFieldBody>,
}

/// The "create a field from this column" form.
///
/// Not [`CreateFieldRequest`] directly, because every member here is optional and that
/// type's `field_type` has a `Default` — so an omitted type and an explicit `"text"`
/// would be indistinguishable, and the inference this module exists to provide could
/// never fire.
#[derive(Debug, Default, Deserialize)]
pub(super) struct NewFieldBody {
    /// Derived from `name`, then from the column header, when absent.
    #[serde(default)]
    pub(super) slug: Option<String>,
    /// Defaults to the column header.
    #[serde(default)]
    pub(super) name: Option<String>,
    /// Inferred from the column's sample values when absent.
    #[serde(default)]
    pub(super) field_type: Option<FieldType>,
    #[serde(default)]
    pub(super) config: Option<FieldConfig>,
    #[serde(default)]
    pub(super) description: Option<String>,
    #[serde(default)]
    pub(super) is_required: bool,
    #[serde(default)]
    pub(super) is_unique: bool,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct MappingBody {
    #[serde(default)]
    pub(super) mappings: Vec<MappingEntryBody>,
    #[serde(default)]
    pub(super) dedupe: ImportDedupe,
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
pub(super) async fn put_mapping(
    State(state): State<AppState>,
    Path(import_id): Path<String>,
    Json(body): Json<MappingBody>,
) -> ApiResult<Json<Value>> {
    let job = load_job(&state, &import_id).await?;
    // `set_import_mapping` refuses an applied job by matching zero rows, which is
    // indistinguishable from "no such job". Answer the honest status here.
    reject_if_applied(&job, ImportStep::Remap)?;

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
pub(super) fn new_field_request(
    body: &NewFieldBody,
    column: &ImportColumn,
) -> ApiResult<CreateFieldRequest> {
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
pub(super) async fn resolve_dedupe(
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
