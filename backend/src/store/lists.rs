impl CrmStore {
    pub async fn list_lists(&self, object_id: Option<&str>) -> Result<Vec<List>> {
        let conn = self.conn.lock().await;
        match object_id.filter(|o| !o.is_empty()) {
            Some(object_ref) => {
                let Some(object) = load_object(&conn, object_ref)? else {
                    return Ok(Vec::new());
                };
                let sql = format!(
                    "SELECT {COLS_LIST} FROM lists WHERE object_id = ?1 ORDER BY position ASC, created_at ASC"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params![object.id], row_to_list)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            }
            None => {
                let sql =
                    format!("SELECT {COLS_LIST} FROM lists ORDER BY position ASC, created_at ASC");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map([], row_to_list)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            }
        }
    }

    pub async fn get_list(&self, list_id: &str) -> Result<Option<List>> {
        let conn = self.conn.lock().await;
        load_list(&conn, list_id)
    }

    pub async fn create_list(&self, req: &CreateListRequest) -> Result<List> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, &req.object_id)? else {
            bail!("unknown object \"{}\"", req.object_id);
        };
        let now = now_rfc3339();
        let id = new_id(ID_LIST);
        let position = next_position(
            &conn,
            "SELECT MAX(position) FROM lists WHERE object_id = ?1",
            &object.id,
        )?;
        conn.execute(
            "INSERT INTO lists (id, object_id, name, description, icon, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![id, object.id, req.name.trim(), req.description, req.icon, position, now],
        )?;
        load_list(&conn, &id)?.context("re-reading the list just created")
    }

    pub async fn update_list(
        &self,
        list_id: &str,
        req: &UpdateListRequest,
    ) -> Result<Option<List>> {
        let conn = self.conn.lock().await;
        let Some(existing) = load_list(&conn, list_id)? else {
            return Ok(None);
        };
        let now = now_rfc3339();
        conn.execute(
            "UPDATE lists SET name = ?2, description = ?3, icon = ?4, position = ?5, updated_at = ?6 WHERE id = ?1",
            params![
                existing.id,
                req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).unwrap_or(&existing.name),
                req.description.as_ref().or(existing.description.as_ref()),
                req.icon.as_ref().or(existing.icon.as_ref()),
                req.position.unwrap_or(existing.position),
                now
            ],
        )?;
        load_list(&conn, &existing.id)
    }

    /// Delete a list, its entries and its list-specific fields. The RECORDS survive:
    /// a list is a set, and removing the set must not remove its members.
    pub async fn delete_list(&self, list_id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        if load_list(&conn, list_id)?.is_none() {
            return Ok(false);
        }
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM list_entries WHERE list_id = ?1",
            params![list_id],
        )?;
        tx.execute("DELETE FROM fields WHERE list_id = ?1", params![list_id])?;
        let n = tx.execute("DELETE FROM lists WHERE id = ?1", params![list_id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    pub async fn list_list_fields(&self, list_id: &str) -> Result<Vec<Field>> {
        let conn = self.conn.lock().await;
        load_list_fields(&conn, list_id)
    }

    /// Add a record to a list, with its list-specific values.
    pub async fn add_list_entry(
        &self,
        list_id: &str,
        req: &AddListEntryRequest,
    ) -> Validated<ListEntry> {
        let conn = self.conn.lock().await;
        let Some(list) = load_list(&conn, list_id)? else {
            bail!("unknown list \"{list_id}\"");
        };
        let Some(record) = load_record(&conn, &req.record_id)? else {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "record_id",
                ValidationCode::BadRelationTarget,
                "no such record",
            )]));
        };
        if record.object_id != list.object_id {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "record_id",
                ValidationCode::BadRelationTarget,
                "that record is not on this list's object",
            )]));
        }
        let list_fields = load_list_fields(&conn, &list.id)?;
        let validated = validate_bag(
            &conn,
            &list.object_id,
            &list_fields,
            &req.values,
            false,
            None,
        )?;
        if !validated.is_ok() {
            return Ok(Err(validated.errors));
        }
        let mut values = validated.values;
        prune_nulls(&mut values);
        let now = now_rfc3339();
        let id = new_id(ID_LIST_ENTRY);
        let position = next_position(
            &conn,
            "SELECT MAX(position) FROM list_entries WHERE list_id = ?1",
            &list.id,
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO list_entries (id, list_id, record_id, data, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, list.id, record.id, encode_json(&values), position, now],
        )?;
        // `INSERT OR IGNORE` means re-adding an existing member is a no-op rather
        // than an error; return whichever row is now the membership.
        let sql = format!(
            "SELECT {COLS_LIST_ENTRY} FROM list_entries WHERE list_id = ?1 AND record_id = ?2"
        );
        let entry = conn
            .query_row(&sql, params![list.id, record.id], row_to_list_entry)
            .optional()?
            .context("re-reading the list entry just written")?;
        Ok(Ok(entry))
    }

    pub async fn update_list_entry(
        &self,
        entry_id: &str,
        req: &UpdateListEntryRequest,
    ) -> Validated<Option<ListEntry>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_LIST_ENTRY} FROM list_entries WHERE id = ?1");
        let Some(existing) = conn
            .query_row(&sql, params![entry_id], row_to_list_entry)
            .optional()?
        else {
            return Ok(Ok(None));
        };
        let Some(list) = load_list(&conn, &existing.list_id)? else {
            bail!("list entry {entry_id} points at a missing list");
        };
        let list_fields = load_list_fields(&conn, &list.id)?;
        let partial = req.mode == UpdateMode::Merge;
        let validated = validate_bag(
            &conn,
            &list.object_id,
            &list_fields,
            &req.values,
            partial,
            None,
        )?;
        if !validated.is_ok() {
            return Ok(Err(validated.errors));
        }
        let mut next = match req.mode {
            UpdateMode::Merge => {
                let mut merged = existing.values.clone();
                for (slug, value) in validated.values {
                    if value.is_null() {
                        merged.remove(&slug);
                    } else {
                        merged.insert(slug, value);
                    }
                }
                merged
            }
            UpdateMode::Replace => validated.values,
        };
        prune_nulls(&mut next);
        let now = now_rfc3339();
        conn.execute(
            "UPDATE list_entries SET data = ?2, updated_at = ?3 WHERE id = ?1",
            params![existing.id, encode_json(&next), now],
        )?;
        Ok(Ok(Some(ListEntry {
            values: next,
            updated_at: now,
            ..existing
        })))
    }

    pub async fn remove_list_entry(&self, entry_id: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn.execute("DELETE FROM list_entries WHERE id = ?1", params![entry_id])?;
        Ok(n > 0)
    }

    pub async fn reorder_list_entries(&self, list_id: &str, ids: &[String]) -> Result<()> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let now = now_rfc3339();
        for (position, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE list_entries SET position = ?2, updated_at = ?3 WHERE id = ?1 AND list_id = ?4",
                params![id, position as i64, now, list_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// One list's entries with their records resolved.
    ///
    /// Filters and sorts may name the record's own fields OR the list's extra
    /// fields; the two namespaces are kept apart by looking a key up in the list
    /// fields first, then in the object's.
    pub async fn query_list_entries(
        &self,
        query: &ListEntryQuery,
        limit: usize,
        offset: usize,
    ) -> Result<ListEntryPage> {
        let conn = self.conn.lock().await;
        let Some(list) = load_list(&conn, &query.list_id)? else {
            return Ok(Page::empty(limit, offset));
        };
        let record_fields = load_fields(&conn, &list.object_id)?;
        let list_fields = load_list_fields(&conn, &list.id)?;
        let record_index = field_index(&record_fields);
        let list_index = field_index(&list_fields);

        let mut params: SqlParams = vec![rusqlite::types::Value::Text(list.id.clone())];
        let mut clauses = vec![
            "e.list_id = ?".to_string(),
            "r.deleted_at IS NULL".to_string(),
        ];
        if let Some(expression) = query.search.as_deref().and_then(fts_match_expression) {
            clauses.push(
                "r.rowid IN (SELECT rowid FROM records_fts WHERE records_fts MATCH ?)".to_string(),
            );
            params.push(rusqlite::types::Value::Text(expression));
        }
        if let Some(filter) = query.filter.as_ref().filter(|f| !f.is_empty()) {
            // A key that names a list field binds to `e`; anything else to `r`.
            clauses.push(build_scoped_filter(
                filter,
                &list_index,
                &record_index,
                &mut params,
            ));
        }
        let where_sql = clauses.join(" AND ");

        let total: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM list_entries e JOIN records r ON r.id = e.record_id WHERE {where_sql}"
            ),
            params_from_iter(params.clone()),
            |r| r.get(0),
        )?;

        let order_by = if query.sorts.is_empty() {
            "e.position ASC, e.id ASC".to_string()
        } else {
            build_scoped_order_by(&query.sorts, &list_index, &record_index)
        };
        let entry_cols = COLS_LIST_ENTRY
            .split(", ")
            .map(|c| format!("e.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let record_cols = COLS_RECORD
            .split(", ")
            .map(|c| format!("r.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {entry_cols}, {record_cols} FROM list_entries e
               JOIN records r ON r.id = e.record_id
              WHERE {where_sql} ORDER BY {order_by} LIMIT ? OFFSET ?"
        );
        params.push(rusqlite::types::Value::Integer(limit as i64));
        params.push(rusqlite::types::Value::Integer(offset as i64));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params), |row| {
            let entry = row_to_list_entry(row)?;
            // The record's columns start after the entry's seven.
            let record = Record {
                id: row.get(7)?,
                object_id: row.get(8)?,
                title: row.get(9)?,
                values: serde_json::from_str::<ValueBag>(&row.get::<_, String>(10)?)
                    .unwrap_or_default(),
                deleted_at: row.get(11)?,
                created_by: row.get(12)?,
                created_at: row.get(13)?,
                updated_at: row.get(14)?,
            };
            Ok(ListEntryView { entry, record })
        })?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page::new(items, total, limit, offset))
    }
}

/// Compile a filter whose keys may name EITHER a list field (bound to `e`) or a
/// record field (bound to `r`).
fn build_scoped_filter(
    filter: &ViewFilter,
    list_index: &HashMap<String, Field>,
    record_index: &HashMap<String, Field>,
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
                .map(|f| build_scoped_filter(f, list_index, record_index, params))
                .filter(|p| p != "1")
                .collect();
            if parts.is_empty() {
                "1".to_string()
            } else {
                format!("({})", parts.join(joiner))
            }
        }
        ViewFilter::Not { filter } => {
            let inner = build_scoped_filter(filter, list_index, record_index, params);
            if inner == "1" {
                "1".to_string()
            } else {
                format!("NOT ({inner})")
            }
        }
        ViewFilter::Condition(condition) => if list_index.contains_key(&condition.field_id) {
            build_condition(condition, list_index, "e", params)
        } else {
            build_condition(condition, record_index, "r", params)
        }
        .unwrap_or_else(|| "1".to_string()),
    }
}

fn build_scoped_order_by(
    sorts: &[ViewSort],
    list_index: &HashMap<String, Field>,
    record_index: &HashMap<String, Field>,
) -> String {
    let mut parts = Vec::new();
    for sort in sorts.iter().take(MAX_SORTS) {
        let resolved = if list_index.contains_key(&sort.field_id) {
            value_expr(list_index, &sort.field_id, "e")
        } else {
            value_expr(record_index, &sort.field_id, "r")
        };
        let Some((expr, field)) = resolved else {
            continue;
        };
        let collate = if field.as_ref().is_none_or(|f| !f.field_type.is_numeric()) {
            " COLLATE NOCASE"
        } else {
            ""
        };
        parts.push(format!(
            "({expr} IS NULL) ASC, {expr}{collate} {}",
            sort.direction.as_sql()
        ));
    }
    parts.push("e.id ASC".to_string());
    parts.join(", ")
}

// ── Activities + tasks ─────────────────────────────────────────────────────────
