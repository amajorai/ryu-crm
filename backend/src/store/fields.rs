impl CrmStore {
    pub async fn list_fields(&self, object_id: &str) -> Result<Vec<Field>> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            return Ok(Vec::new());
        };
        load_fields(&conn, &object.id)
    }

    pub async fn get_field(&self, field_id: &str) -> Result<Option<Field>> {
        let conn = self.conn.lock().await;
        load_field(&conn, field_id)
    }

    /// Resolve a field on one object by id OR slug. The tolerant lookup every
    /// filter, sort, import mapping and agent tool call goes through.
    pub async fn resolve_field(&self, object_id: &str, id_or_slug: &str) -> Result<Option<Field>> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            return Ok(None);
        };
        let fields = load_fields(&conn, &object.id)?;
        Ok(fields
            .into_iter()
            .find(|f| f.id == id_or_slug || f.slug == id_or_slug))
    }

    /// Add a field to an object (`list_id = None`) or to a list (`Some`).
    pub async fn create_field(
        &self,
        object_id: &str,
        list_id: Option<&str>,
        req: &CreateFieldRequest,
    ) -> Validated<Field> {
        let slug = req.slug.trim().to_lowercase();
        let mut errors = Vec::new();
        if !is_valid_slug(&slug) {
            errors.push(FieldValidationError::coded(
                "",
                "slug",
                ValidationCode::Invalid,
                "a field slug must start with a lowercase letter and contain only lowercase letters, digits and underscores, and must not be one of the reserved names",
            ));
        }
        if req.name.trim().is_empty() {
            errors.push(FieldValidationError::coded(
                "",
                "name",
                ValidationCode::Required,
                "a field name is required",
            ));
        }
        if let Some(error) = validate_config(&slug, req.field_type, &req.config) {
            errors.push(error);
        }
        // Rejected rather than documented-as-inert: `validate_bag` enforces uniqueness
        // with a `SELECT … FROM records`, but a list field's values live in
        // `list_entries.data`, so a unique list field would enforce NOTHING, forever,
        // with no error anywhere. A guard is one line; a silent lie the UI can switch
        // on is a support ticket nobody can reproduce.
        if req.is_unique && list_id.is_some() {
            errors.push(FieldValidationError::coded(
                "",
                &slug,
                ValidationCode::Invalid,
                "list-specific fields cannot be unique",
            ));
        }
        if !errors.is_empty() {
            return Ok(Err(errors));
        }

        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            bail!("unknown object \"{object_id}\"");
        };
        let taken = match list_id {
            Some(list) => load_list_fields(&conn, list)?
                .iter()
                .any(|f| f.slug == slug),
            None => load_fields(&conn, &object.id)?
                .iter()
                .any(|f| f.slug == slug),
        };
        if taken {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                &slug,
                ValidationCode::NotUnique,
                format!("a field with the slug \"{slug}\" already exists here"),
            )]));
        }

        let mut config = req.config.clone();
        assign_option_ids(&slug, &mut config);
        let now = now_rfc3339();
        let id = new_id(ID_FIELD);
        let position = match req.position {
            Some(p) => p,
            None => match list_id {
                Some(list) => next_position(
                    &conn,
                    "SELECT MAX(position) FROM fields WHERE list_id = ?1",
                    list,
                )?,
                None => next_position(
                    &conn,
                    "SELECT MAX(position) FROM fields WHERE object_id = ?1 AND list_id IS NULL",
                    &object.id,
                )?,
            },
        };
        conn.execute(
            "INSERT INTO fields
               (id, object_id, list_id, slug, name, field_type, config, description,
                is_required, is_unique, is_system, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, ?11, ?12, ?12)",
            params![
                id,
                object.id,
                list_id,
                slug,
                req.name.trim(),
                req.field_type.as_str(),
                config.encode(),
                req.description,
                i64::from(req.is_required),
                i64::from(req.is_unique),
                position,
                now
            ],
        )?;
        let field = load_field(&conn, &id)?.context("re-reading the field just created")?;
        Ok(Ok(field))
    }

    /// Rename / reconfigure a field. `slug` and `field_type` are immutable — see the
    /// models module docs.
    pub async fn update_field(
        &self,
        field_id: &str,
        req: &UpdateFieldRequest,
    ) -> Validated<Option<Field>> {
        let conn = self.conn.lock().await;
        let Some(existing) = load_field(&conn, field_id)? else {
            return Ok(Ok(None));
        };
        let mut config = req
            .config
            .clone()
            .unwrap_or_else(|| existing.config.clone());
        if let Some(error) = validate_config(&existing.slug, existing.field_type, &config) {
            return Ok(Err(vec![error]));
        }
        assign_option_ids(&existing.slug, &mut config);
        let now = now_rfc3339();
        conn.execute(
            "UPDATE fields SET name = ?2, config = ?3, description = ?4, is_required = ?5,
                               is_unique = ?6, position = ?7, updated_at = ?8
             WHERE id = ?1",
            params![
                existing.id,
                req.name
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .unwrap_or(&existing.name),
                config.encode(),
                req.description.as_ref().or(existing.description.as_ref()),
                i64::from(req.is_required.unwrap_or(existing.is_required)),
                i64::from(req.is_unique.unwrap_or(existing.is_unique)),
                req.position.unwrap_or(existing.position),
                now
            ],
        )?;
        Ok(Ok(load_field(&conn, &existing.id)?))
    }

    /// Delete a field and strip its values from every record.
    ///
    /// Refuses a system field. The value strip is not optional housekeeping: a bag
    /// entry whose field is gone is invisible in the UI, still matched by FTS, and
    /// would silently reappear if a new field ever took the same slug.
    pub async fn delete_field(&self, field_id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        let Some(field) = load_field(&conn, field_id)? else {
            return Ok(false);
        };
        if field.is_system {
            bail!("the system field \"{}\" cannot be deleted", field.slug);
        }
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM fields WHERE id = ?1", params![field.id])?;
        tx.execute(
            "DELETE FROM record_links WHERE field_id = ?1",
            params![field.id],
        )?;
        match field.list_id.as_deref() {
            Some(list_id) => {
                tx.execute(
                    &format!("UPDATE list_entries SET data = json_remove(data, '$.{}') WHERE list_id = ?1", field.slug),
                    params![list_id],
                )?;
            }
            None => {
                tx.execute(
                    &format!(
                        "UPDATE records SET data = json_remove(data, '$.{}') WHERE object_id = ?1",
                        field.slug
                    ),
                    params![field.object_id],
                )?;
                // A view that still lists this column, or groups by it, would render
                // a ghost. Both are cheap to repair here and impossible to notice
                // later.
                tx.execute(
                    "UPDATE views SET group_by_field_id = NULL WHERE group_by_field_id = ?1",
                    params![field.id],
                )?;
                if field.field_type.is_searchable() {
                    reindex_object(&tx, &field.object_id)?;
                }
            }
        }
        tx.commit()?;
        Ok(true)
    }

    /// Give the listed field ids positions `0..n`. Ids not listed keep their relative
    /// order after them.
    pub async fn reorder_fields(
        &self,
        object_id: &str,
        list_id: Option<&str>,
        ids: &[String],
    ) -> Result<()> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let now = now_rfc3339();
        for (position, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE fields SET position = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, position as i64, now],
            )?;
        }
        // Push everything unlisted past the explicit block, preserving its order.
        let offset = ids.len() as i64;
        match list_id {
            Some(list) => tx.execute(
                "UPDATE fields SET position = position + ?2 WHERE list_id = ?1 AND id NOT IN (SELECT value FROM json_each(?3))",
                params![list, offset, encode_json(&ids)],
            )?,
            None => tx.execute(
                "UPDATE fields SET position = position + ?2 WHERE object_id = ?1 AND list_id IS NULL AND id NOT IN (SELECT value FROM json_each(?3))",
                params![object_id, offset, encode_json(&ids)],
            )?,
        };
        tx.commit()?;
        Ok(())
    }
}

/// Type-specific config sanity, run before a field is written. Returns the first
/// problem, because a config with two problems is one badly-filled form.
fn validate_config(
    slug: &str,
    field_type: FieldType,
    config: &FieldConfig,
) -> Option<FieldValidationError> {
    if field_type.is_option_backed() {
        let mut seen = HashSet::new();
        for option in &config.options {
            if option.label.trim().is_empty() {
                return Some(FieldValidationError::coded(
                    "",
                    slug,
                    ValidationCode::Invalid,
                    "every option needs a label",
                ));
            }
            if !option.id.is_empty() && !seen.insert(option.id.clone()) {
                return Some(FieldValidationError::coded(
                    "",
                    slug,
                    ValidationCode::NotUnique,
                    format!("duplicate option id \"{}\"", option.id),
                ));
            }
        }
    }
    if field_type == FieldType::Relation
        && config
            .relation_object_id
            .as_deref()
            .is_none_or(|t| t.trim().is_empty())
    {
        return Some(FieldValidationError::coded(
            "",
            slug,
            ValidationCode::BadRelationTarget,
            "a relation field needs a target object",
        ));
    }
    None
}

/// Give every option without an id a deterministic one derived from the field slug
/// and label, and normalize positions to `0..n`.
///
/// Derived rather than random so the same option added twice on two machines does
/// not produce two ids for one concept, and so a seeded option keeps the id the
/// panel hardcodes.
fn assign_option_ids(field_slug: &str, config: &mut FieldConfig) {
    let mut taken: HashSet<String> = config
        .options
        .iter()
        .filter(|o| !o.id.is_empty())
        .map(|o| o.id.clone())
        .collect();
    for (position, option) in config.options.iter_mut().enumerate() {
        option.position = position as i64;
        if !option.id.is_empty() {
            continue;
        }
        let base = slugify(&option.label).unwrap_or_else(|| "option".to_string());
        let mut candidate = format!("{ID_OPTION}{field_slug}_{base}");
        let mut n = 2;
        while taken.contains(&candidate) {
            candidate = format!("{ID_OPTION}{field_slug}_{base}_{n}");
            n += 1;
        }
        taken.insert(candidate.clone());
        option.id = candidate;
    }
}

/// Rebuild the FTS rows for one object. Used after a schema change that alters what
/// is searchable.
pub(super) fn reindex_object(conn: &Connection, object_id: &str) -> Result<usize> {
    let fields = load_fields(conn, object_id)?;
    let sql = format!("SELECT {COLS_RECORD} FROM records WHERE object_id = ?1");
    let records = {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![object_id], row_to_record)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for record in &records {
        fts_reindex(
            conn,
            &record.id,
            &record.title,
            &fts_body(&fields, &record.values),
        )?;
    }
    Ok(records.len())
}

// ── Query builder ──────────────────────────────────────────────────────────────
//
// Every filter/sort in this app funnels through here. Two rules make it safe:
//
//   * User VALUES are always bound parameters, never interpolated.
//   * The only interpolated text is a field SLUG, and `models::is_valid_slug`
//     restricts slugs to `[a-z][a-z0-9_]*` — there is no quote, `$`, `.` or `[` in
//     the alphabet, so a JSON path built from one cannot be broken out of. That is
//     the whole reason the slug validator is as strict as it is.

pub(super) type SqlParams = Vec<rusqlite::types::Value>;

fn sql_value(value: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as S;
    match value {
        Value::Null => S::Null,
        Value::Bool(b) => S::Integer(i64::from(*b)),
        Value::Number(n) => n
            .as_i64()
            .map(S::Integer)
            .or_else(|| n.as_f64().map(S::Real))
            .unwrap_or(S::Null),
        Value::String(s) => S::Text(s.clone()),
        other => S::Text(other.to_string()),
    }
}

/// The SQL expression that yields one field's value, for a row aliased `alias`.
/// `None` when the key names nothing.
pub(super) fn value_expr(
    index: &HashMap<String, Field>,
    key: &str,
    alias: &str,
) -> Option<(String, Option<Field>)> {
    if ViewSort::is_intrinsic(key) {
        return Some((format!("{alias}.{key}"), None));
    }
    let field = index.get(key)?;
    Some((
        format!("json_extract({alias}.data, '$.{}')", field.slug),
        Some(field.clone()),
    ))
}

/// One leaf condition. Returns `None` for a condition naming an unknown field, which
/// the caller treats as "no constraint" — a saved view whose field was deleted must
/// degrade to showing everything, not to a 500.
pub(super) fn build_condition(
    condition: &FilterCondition,
    index: &HashMap<String, Field>,
    alias: &str,
    params: &mut SqlParams,
) -> Option<String> {
    let (expr, field) = value_expr(index, &condition.field_id, alias)?;
    let multi = field.as_ref().is_some_and(|f| f.field_type.is_multi());
    let numeric = field.as_ref().is_some_and(|f| f.field_type.is_numeric());

    // Membership over a stored JSON array.
    let any_of = |values: &[Value], params: &mut SqlParams| -> String {
        if values.is_empty() {
            return "0".to_string();
        }
        let placeholders = values
            .iter()
            .map(|v| {
                params.push(sql_value(v));
                "?"
            })
            .collect::<Vec<_>>()
            .join(", ");
        if multi {
            format!(
                "EXISTS (SELECT 1 FROM json_each({expr}) je WHERE je.value IN ({placeholders}))"
            )
        } else {
            format!("{expr} IN ({placeholders})")
        }
    };

    let scalar = |params: &mut SqlParams| {
        params.push(sql_value(&condition.value));
    };

    let sql = match condition.op {
        FilterOperator::Eq => {
            if multi {
                any_of(std::slice::from_ref(&condition.value), params)
            } else {
                scalar(params);
                format!("{expr} = ?")
            }
        }
        FilterOperator::NotEq => {
            if multi {
                let inner = any_of(std::slice::from_ref(&condition.value), params);
                format!("NOT ({inner})")
            } else {
                scalar(params);
                format!("({expr} IS NULL OR {expr} <> ?)")
            }
        }
        FilterOperator::Contains => {
            scalar(params);
            format!("lower(CAST({expr} AS TEXT)) LIKE '%' || lower(?) || '%'")
        }
        FilterOperator::NotContains => {
            scalar(params);
            format!(
                "({expr} IS NULL OR lower(CAST({expr} AS TEXT)) NOT LIKE '%' || lower(?) || '%')"
            )
        }
        FilterOperator::StartsWith => {
            scalar(params);
            format!("lower(CAST({expr} AS TEXT)) LIKE lower(?) || '%'")
        }
        FilterOperator::EndsWith => {
            scalar(params);
            format!("lower(CAST({expr} AS TEXT)) LIKE '%' || lower(?)")
        }
        FilterOperator::Gt | FilterOperator::Gte | FilterOperator::Lt | FilterOperator::Lte => {
            let op = match condition.op {
                FilterOperator::Gt => ">",
                FilterOperator::Gte => ">=",
                FilterOperator::Lt => "<",
                _ => "<=",
            };
            scalar(params);
            // Text comparison is lexicographic, which is CORRECT for this app's
            // dates because they are fixed-width RFC-3339 (see models::now_rfc3339).
            if numeric {
                format!("CAST({expr} AS REAL) {op} CAST(? AS REAL)")
            } else {
                format!("{expr} {op} ?")
            }
        }
        FilterOperator::Between => {
            let bounds = condition.value.as_array().cloned().unwrap_or_default();
            if bounds.len() != 2 {
                return None;
            }
            params.push(sql_value(&bounds[0]));
            params.push(sql_value(&bounds[1]));
            if numeric {
                format!("CAST({expr} AS REAL) BETWEEN CAST(? AS REAL) AND CAST(? AS REAL)")
            } else {
                format!("{expr} BETWEEN ? AND ?")
            }
        }
        FilterOperator::IsEmpty => {
            if multi {
                format!("({expr} IS NULL OR json_array_length({expr}) = 0)")
            } else {
                format!("({expr} IS NULL OR CAST({expr} AS TEXT) = '')")
            }
        }
        FilterOperator::IsNotEmpty => {
            if multi {
                format!("({expr} IS NOT NULL AND json_array_length({expr}) > 0)")
            } else {
                format!("({expr} IS NOT NULL AND CAST({expr} AS TEXT) <> '')")
            }
        }
        FilterOperator::IsAnyOf => {
            let values = condition
                .value
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![condition.value.clone()]);
            any_of(&values, params)
        }
        FilterOperator::IsNoneOf => {
            let values = condition
                .value
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![condition.value.clone()]);
            let inner = any_of(&values, params);
            format!("NOT ({inner})")
        }
        FilterOperator::IsTrue => format!("{expr} = 1"),
        FilterOperator::IsFalse => format!("({expr} IS NULL OR {expr} = 0)"),
    };
    Some(sql)
}

/// Compile a filter tree. Always returns a valid boolean expression; an empty node
/// compiles to `1`.
pub(super) fn build_filter(
    filter: &ViewFilter,
    index: &HashMap<String, Field>,
    alias: &str,
    params: &mut SqlParams,
) -> String {
    match filter {
        ViewFilter::And { filters } | ViewFilter::Or { filters } => {
            let joiner = if matches!(filter, ViewFilter::And { .. }) {
                " AND "
            } else {
                " OR "
            };
            let parts: Vec<String> = filters
                .iter()
                .map(|f| build_filter(f, index, alias, params))
                .filter(|p| p != "1")
                .collect();
            if parts.is_empty() {
                "1".to_string()
            } else {
                format!("({})", parts.join(joiner))
            }
        }
        ViewFilter::Not { filter } => {
            let inner = build_filter(filter, index, alias, params);
            if inner == "1" {
                "1".to_string()
            } else {
                format!("NOT ({inner})")
            }
        }
        ViewFilter::Condition(condition) => {
            build_condition(condition, index, alias, params).unwrap_or_else(|| "1".to_string())
        }
    }
}

/// Compile a sort list into an `ORDER BY` body, always ending in `id` so pagination
/// cannot repeat or skip a row when two rows tie.
pub(super) fn build_order_by(
    sorts: &[ViewSort],
    index: &HashMap<String, Field>,
    alias: &str,
) -> String {
    let mut parts = Vec::new();
    for sort in sorts.iter().take(MAX_SORTS) {
        let Some((expr, field)) = value_expr(index, &sort.field_id, alias) else {
            continue;
        };
        let text = field.as_ref().is_none_or(|f| !f.field_type.is_numeric());
        let collate = if text { " COLLATE NOCASE" } else { "" };
        // NULLs last in both directions: an unset field is not "smallest", it is
        // absent, and a table whose blank rows float to the top is unusable.
        parts.push(format!(
            "({expr} IS NULL) ASC, {expr}{collate} {}",
            sort.direction.as_sql()
        ));
    }
    parts.push(format!("{alias}.id ASC"));
    parts.join(", ")
}

/// More than this many sorts is a UI bug, and each one is an unindexed
/// `json_extract` on every candidate row.
pub(super) const MAX_SORTS: usize = 4;

// ── Records ────────────────────────────────────────────────────────────────────
