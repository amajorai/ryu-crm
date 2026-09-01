impl CrmStore {
    pub async fn list_views(&self, object_id: &str) -> Result<Vec<View>> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            return Ok(Vec::new());
        };
        load_views(&conn, &object.id)
    }

    pub async fn get_view(&self, view_id: &str) -> Result<Option<View>> {
        let conn = self.conn.lock().await;
        load_view(&conn, view_id)
    }

    pub async fn create_view(&self, object_id: &str, req: &CreateViewRequest) -> Result<View> {
        let mut conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            bail!("unknown object \"{object_id}\"");
        };
        let tx = conn.transaction()?;
        let now = now_rfc3339();
        let id = new_id(ID_VIEW);
        let position = next_position(
            &tx,
            "SELECT MAX(position) FROM views WHERE object_id = ?1",
            &object.id,
        )?;
        if req.is_default {
            tx.execute(
                "UPDATE views SET is_default = 0 WHERE object_id = ?1",
                params![object.id],
            )?;
        }
        tx.execute(
            "INSERT INTO views
               (id, object_id, name, kind, filter, sorts, visible_fields, group_by_field_id,
                is_default, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            params![
                id,
                object.id,
                req.name.trim(),
                req.kind.as_str(),
                req.filter.as_ref().map(encode_json),
                encode_json(&req.sorts),
                encode_json(&req.visible_field_ids),
                req.group_by_field_id,
                i64::from(req.is_default),
                position,
                now
            ],
        )?;
        let view = load_view(&tx, &id)?.context("re-reading the view just created")?;
        tx.commit()?;
        Ok(view)
    }

    pub async fn update_view(
        &self,
        view_id: &str,
        req: &UpdateViewRequest,
    ) -> Result<Option<View>> {
        let conn = self.conn.lock().await;
        let Some(existing) = load_view(&conn, view_id)? else {
            return Ok(None);
        };
        let now = now_rfc3339();
        conn.execute(
            "UPDATE views SET name = ?2, kind = ?3, filter = ?4, sorts = ?5, visible_fields = ?6,
                              group_by_field_id = ?7, position = ?8, updated_at = ?9
             WHERE id = ?1",
            params![
                existing.id,
                req.name
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .unwrap_or(&existing.name),
                req.kind.unwrap_or(existing.kind).as_str(),
                req.filter
                    .as_ref()
                    .or(existing.filter.as_ref())
                    .map(encode_json),
                encode_json(req.sorts.as_ref().unwrap_or(&existing.sorts)),
                encode_json(
                    req.visible_field_ids
                        .as_ref()
                        .unwrap_or(&existing.visible_field_ids)
                ),
                req.group_by_field_id
                    .as_ref()
                    .or(existing.group_by_field_id.as_ref()),
                req.position.unwrap_or(existing.position),
                now
            ],
        )?;
        load_view(&conn, &existing.id)
    }

    /// Delete a view. Refuses the last one on an object — an object with no view has
    /// no way to open it.
    pub async fn delete_view(&self, view_id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        let Some(view) = load_view(&conn, view_id)? else {
            return Ok(false);
        };
        let remaining: i64 = conn.query_row(
            "SELECT COUNT(*) FROM views WHERE object_id = ?1",
            params![view.object_id],
            |r| r.get(0),
        )?;
        if remaining <= 1 {
            bail!("an object must keep at least one view");
        }
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM views WHERE id = ?1", params![view.id])?;
        if view.is_default {
            // Promote the next one rather than leaving the object with no default.
            tx.execute(
                "UPDATE views SET is_default = 1 WHERE id = (
                     SELECT id FROM views WHERE object_id = ?1 ORDER BY position ASC LIMIT 1)",
                params![view.object_id],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub async fn set_default_view(&self, view_id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        let Some(view) = load_view(&conn, view_id)? else {
            return Ok(false);
        };
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE views SET is_default = 0 WHERE object_id = ?1",
            params![view.object_id],
        )?;
        tx.execute(
            "UPDATE views SET is_default = 1 WHERE id = ?1",
            params![view.id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Run a saved view: the flat page always, plus board columns when the view is a
    /// board with a valid grouping field.
    pub async fn run_view(
        &self,
        view_id: &str,
        overrides: &ViewQueryOverrides,
        limit: usize,
        offset: usize,
    ) -> Result<Option<ViewResult>> {
        let conn = self.conn.lock().await;
        let Some(view) = load_view(&conn, view_id)? else {
            return Ok(None);
        };
        let Some(object) = load_object(&conn, &view.object_id)? else {
            return Ok(None);
        };
        let all_fields = load_fields(&conn, &object.id)?;
        let index = field_index(&all_fields);
        let fields: Vec<Field> = if view.visible_field_ids.is_empty() {
            all_fields.clone()
        } else {
            view.visible_field_ids
                .iter()
                .filter_map(|id| index.get(id.as_str()).cloned())
                .collect()
        };

        // The view's own filter is ANDed with the override, never replaced — see
        // `ViewQueryOverrides::filter`.
        let filter = match (view.filter.clone(), overrides.filter.clone()) {
            (Some(saved), Some(extra)) => Some(ViewFilter::And {
                filters: vec![saved, extra],
            }),
            (Some(saved), None) => Some(saved),
            (None, extra) => extra,
        };
        let sorts = overrides
            .sorts
            .clone()
            .unwrap_or_else(|| view.sorts.clone());
        let query = RecordQuery {
            object_id: object.id.clone(),
            filter,
            sorts,
            search: overrides.search.clone(),
            include_deleted: overrides.include_deleted,
            ..Default::default()
        };
        let (where_sql, mut params) = build_record_where(&query, &object.id, &index);
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM records r WHERE {where_sql}"),
            params_from_iter(params.clone()),
            |r| r.get(0),
        )?;
        let order_by = build_order_by(&query.sorts, &index, "r");
        let items = {
            let sql = format!(
                "SELECT {COLS_RECORD} FROM records r WHERE {where_sql} ORDER BY {order_by} LIMIT ? OFFSET ?"
            );
            params.push(rusqlite::types::Value::Integer(limit as i64));
            params.push(rusqlite::types::Value::Integer(offset as i64));
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(params), row_to_record)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let page = Page::new(items, total, limit, offset);

        let groups = if view.kind == ViewKind::Board {
            build_board_groups(&conn, &view, &object, &all_fields, &index, &query, limit)?
        } else {
            None
        };

        Ok(Some(ViewResult {
            view,
            fields,
            page,
            groups,
        }))
    }
}

/// One query per column. `N` is the option count (under twenty in practice), and the
/// alternative — one grouped query plus a windowed per-group limit — is materially
/// harder to read for no measurable gain at CRM scale.
fn build_board_groups(
    conn: &Connection,
    view: &View,
    object: &Object,
    all_fields: &[Field],
    index: &HashMap<String, Field>,
    base: &RecordQuery,
    per_group: usize,
) -> Result<Option<Vec<BoardGroup>>> {
    let Some(group_field) = view
        .group_by_field_id
        .as_deref()
        .and_then(|id| index.get(id))
        .filter(|f| f.field_type.is_option_backed())
    else {
        // A board mid-configuration must degrade, not 500.
        return Ok(None);
    };
    let value_field = all_fields
        .iter()
        .find(|f| f.field_type == FieldType::Currency)
        .cloned();

    let mut groups = Vec::new();
    let mut buckets: Vec<(Option<SelectOption>, i64)> = group_field
        .config
        .sorted_options()
        .into_iter()
        .map(|o| {
            let position = o.position;
            (Some(o), position)
        })
        .collect();
    // The "no value" column always exists and always sorts last: records with no
    // stage are the ones that need attention, and hiding them loses them.
    buckets.push((None, i64::MAX));

    for (option, position) in buckets {
        let condition = FilterCondition {
            field_id: group_field.id.clone(),
            op: match &option {
                Some(_) => FilterOperator::Eq,
                None => FilterOperator::IsEmpty,
            },
            value: option.as_ref().map(|o| json!(o.id)).unwrap_or(Value::Null),
        };
        let filter = match base.filter.clone() {
            Some(existing) => ViewFilter::And {
                filters: vec![existing, ViewFilter::Condition(condition)],
            },
            None => ViewFilter::Condition(condition),
        };
        let query = RecordQuery {
            filter: Some(filter),
            ..base.clone()
        };
        let (where_sql, mut params) = build_record_where(&query, &object.id, index);
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM records r WHERE {where_sql}"),
            params_from_iter(params.clone()),
            |r| r.get(0),
        )?;
        let value_cents = match value_field.as_ref() {
            Some(field) => {
                let sum: Option<i64> = conn.query_row(
                    &format!(
                        "SELECT CAST(SUM(COALESCE(json_extract(r.data, '$.{}'), 0)) AS INTEGER)
                           FROM records r WHERE {where_sql}",
                        field.slug
                    ),
                    params_from_iter(params.clone()),
                    |r| r.get(0),
                )?;
                Some(sum.unwrap_or(0))
            }
            None => None,
        };
        let order_by = build_order_by(&query.sorts, index, "r");
        let records = {
            let sql = format!(
                "SELECT {COLS_RECORD} FROM records r WHERE {where_sql} ORDER BY {order_by} LIMIT ?"
            );
            params.push(rusqlite::types::Value::Integer(per_group as i64));
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(params), row_to_record)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        groups.push(BoardGroup {
            option_id: option.as_ref().map(|o| o.id.clone()),
            label: option
                .as_ref()
                .map(|o| o.label.clone())
                .unwrap_or_else(|| "No value".to_string()),
            color: option.as_ref().and_then(|o| o.color.clone()),
            position,
            total,
            value_cents,
            records,
        });
    }
    Ok(Some(groups))
}

// ── Lists ──────────────────────────────────────────────────────────────────────
