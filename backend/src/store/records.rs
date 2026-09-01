impl CrmStore {
    pub async fn get_record(&self, record_id: &str) -> Result<Option<Record>> {
        let conn = self.conn.lock().await;
        load_record(&conn, record_id)
    }

    /// Everything the record drawer renders, in one lock.
    pub async fn get_record_detail(&self, record_id: &str) -> Result<Option<RecordDetail>> {
        let conn = self.conn.lock().await;
        let Some(record) = load_record(&conn, record_id)? else {
            return Ok(None);
        };
        let Some(object) = load_object(&conn, &record.object_id)? else {
            return Ok(None);
        };
        let fields = load_fields(&conn, &object.id)?;
        let links = link_views(&conn, &record.id)?;
        let activities = {
            let sql = format!(
                "SELECT {COLS_ACTIVITY} FROM activities WHERE record_id = ?1
                 ORDER BY created_at DESC, id DESC LIMIT ?2"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![record.id, RecordDetail::TIMELINE_LIMIT as i64],
                row_to_activity,
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let lists = {
            let mut stmt = conn.prepare(
                "SELECT l.id, l.name, e.id FROM list_entries e
                 JOIN lists l ON l.id = e.list_id
                 WHERE e.record_id = ?1 ORDER BY l.position ASC",
            )?;
            let rows = stmt.query_map(params![record.id], |row| {
                Ok(ListMembership {
                    list_id: row.get(0)?,
                    list_name: row.get(1)?,
                    entry_id: row.get(2)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(Some(RecordDetail {
            record,
            object,
            fields,
            links,
            activities,
            lists,
        }))
    }

    /// Create a record.
    ///
    /// `object_id` must already be resolved by the caller — a handler resolves the
    /// object anyway (it needs it for the event payload), and having the store
    /// re-resolve it would double every lookup. An unknown id is an internal error,
    /// not a validation failure.
    pub async fn create_record(
        &self,
        object_id: &str,
        req: &CreateRecordRequest,
    ) -> Validated<Record> {
        let mut conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            bail!("unknown object \"{object_id}\"");
        };
        let fields = load_fields(&conn, &object.id)?;
        let mut incoming = req.values.clone();
        apply_defaults(&fields, &mut incoming);
        let validated = validate_bag(&conn, &object.id, &fields, &incoming, false, None)?;
        if !validated.is_ok() {
            return Ok(Err(validated.errors));
        }
        let mut values = validated.values;
        prune_nulls(&mut values);

        let tx = conn.transaction()?;
        let record = insert_record(&tx, &object, &fields, values, req.created_by.as_deref())?;
        tx.commit()?;
        Ok(Ok(record))
    }

    /// Update a record's values. `Ok(Ok(None))` = no such record.
    ///
    /// Returns the diff alongside the row; an empty `changed` means the write was a
    /// no-op and the caller must NOT emit `record.updated`.
    pub async fn update_record(
        &self,
        record_id: &str,
        req: &UpdateRecordRequest,
    ) -> Validated<Option<RecordUpdate>> {
        let mut conn = self.conn.lock().await;
        let Some(existing) = load_record(&conn, record_id)? else {
            return Ok(Ok(None));
        };
        let Some(object) = load_object(&conn, &existing.object_id)? else {
            bail!("record {record_id} points at a missing object");
        };
        let fields = load_fields(&conn, &object.id)?;
        let partial = req.mode == UpdateMode::Merge;
        let validated = validate_bag(
            &conn,
            &object.id,
            &fields,
            &req.values,
            partial,
            Some(&existing.id),
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

        let tx = conn.transaction()?;
        let update = write_record_values(&tx, &object, &fields, &existing, next)?;
        tx.commit()?;
        Ok(Ok(Some(update)))
    }

    /// Soft delete. The row survives so its timeline, links and list memberships
    /// stay explicable and so an accidental delete is one restore away.
    pub async fn delete_record(&self, record_id: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let now = now_rfc3339();
        let n = conn.execute(
            "UPDATE records SET deleted_at = ?2, updated_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![record_id, now],
        )?;
        Ok(n > 0)
    }

    pub async fn restore_record(&self, record_id: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let now = now_rfc3339();
        let n = conn.execute(
            "UPDATE records SET deleted_at = NULL, updated_at = ?2 WHERE id = ?1 AND deleted_at IS NOT NULL",
            params![record_id, now],
        )?;
        Ok(n > 0)
    }

    /// Hard delete, with the ordered cascade. Irreversible.
    pub async fn purge_record(&self, record_id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        if load_record(&conn, record_id)?.is_none() {
            return Ok(false);
        }
        let tx = conn.transaction()?;
        fts_delete(&tx, record_id)?;
        tx.execute(
            "DELETE FROM record_links WHERE source_record_id = ?1 OR target_record_id = ?1",
            params![record_id],
        )?;
        tx.execute(
            "DELETE FROM list_entries WHERE record_id = ?1",
            params![record_id],
        )?;
        tx.execute(
            "DELETE FROM activities WHERE record_id = ?1",
            params![record_id],
        )?;
        let n = tx.execute("DELETE FROM records WHERE id = ?1", params![record_id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// The one paginated record query. Filters, sorts, FTS pre-filter, list scoping
    /// and explicit id sets all compose here.
    pub async fn query_records(
        &self,
        query: &RecordQuery,
        limit: usize,
        offset: usize,
    ) -> Result<RecordPage> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, &query.object_id)? else {
            return Ok(Page::empty(limit, offset));
        };
        let fields = load_fields(&conn, &object.id)?;
        let index = field_index(&fields);
        let (where_sql, mut params) = build_record_where(query, &object.id, &index);

        let count_sql = format!("SELECT COUNT(*) FROM records r WHERE {where_sql}");
        let total: i64 =
            conn.query_row(&count_sql, params_from_iter(params.clone()), |r| r.get(0))?;

        let order_by = build_order_by(&query.sorts, &index, "r");
        let sql = format!(
            "SELECT {COLS_RECORD} FROM records r WHERE {where_sql} ORDER BY {order_by} LIMIT ? OFFSET ?"
        );
        params.push(rusqlite::types::Value::Integer(limit as i64));
        params.push(rusqlite::types::Value::Integer(offset as i64));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params), row_to_record)?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page::new(items, total, limit, offset))
    }

    /// Count only — the board's per-column totals and the summary strip.
    pub async fn count_records(&self, query: &RecordQuery) -> Result<i64> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, &query.object_id)? else {
            return Ok(0);
        };
        let fields = load_fields(&conn, &object.id)?;
        let index = field_index(&fields);
        let (where_sql, params) = build_record_where(query, &object.id, &index);
        let sql = format!("SELECT COUNT(*) FROM records r WHERE {where_sql}");
        Ok(conn.query_row(&sql, params_from_iter(params), |r| r.get(0))?)
    }

    /// Validate a bag without writing. The panel's inline-edit path calls this to
    /// show an error before it commits a cell.
    pub async fn validate_values(
        &self,
        object_id: &str,
        values: &ValueBag,
        partial: bool,
        exclude_record_id: Option<&str>,
    ) -> Result<ValidatedValues> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            bail!("unknown object \"{object_id}\"");
        };
        let fields = load_fields(&conn, &object.id)?;
        validate_bag(
            &conn,
            &object.id,
            &fields,
            values,
            partial,
            exclude_record_id,
        )
    }
}

/// Assemble the `WHERE` body shared by `query_records` and `count_records`, so the
/// count can never disagree with the page it describes.
pub(super) fn build_record_where(
    query: &RecordQuery,
    object_id: &str,
    index: &HashMap<String, Field>,
) -> (String, SqlParams) {
    let mut params: SqlParams = Vec::new();
    let mut clauses = vec!["r.object_id = ?".to_string()];
    params.push(rusqlite::types::Value::Text(object_id.to_string()));

    if !query.include_deleted {
        clauses.push("r.deleted_at IS NULL".to_string());
    }
    if let Some(list_id) = query.list_id.as_deref().filter(|l| !l.is_empty()) {
        clauses.push(
            "EXISTS (SELECT 1 FROM list_entries le WHERE le.record_id = r.id AND le.list_id = ?)"
                .to_string(),
        );
        params.push(rusqlite::types::Value::Text(list_id.to_string()));
    }
    if let Some(ids) = query.record_ids.as_ref() {
        if ids.is_empty() {
            clauses.push("0".to_string());
        } else {
            let placeholders = ids
                .iter()
                .map(|id| {
                    params.push(rusqlite::types::Value::Text(id.clone()));
                    "?"
                })
                .collect::<Vec<_>>()
                .join(", ");
            clauses.push(format!("r.id IN ({placeholders})"));
        }
    }
    if let Some(expression) = query.search.as_deref().and_then(fts_match_expression) {
        clauses.push(
            "r.rowid IN (SELECT rowid FROM records_fts WHERE records_fts MATCH ?)".to_string(),
        );
        params.push(rusqlite::types::Value::Text(expression));
    }
    if let Some(filter) = query.filter.as_ref().filter(|f| !f.is_empty()) {
        clauses.push(build_filter(filter, index, "r", &mut params));
    }
    (clauses.join(" AND "), params)
}

/// Insert one record, maintaining links and the FTS index. Takes an ALREADY
/// VALIDATED bag.
pub(super) fn insert_record(
    conn: &Connection,
    object: &Object,
    fields: &[Field],
    values: ValueBag,
    created_by: Option<&str>,
) -> Result<Record> {
    let now = now_rfc3339();
    let record = Record {
        id: new_id(ID_RECORD),
        object_id: object.id.clone(),
        title: compute_title(object, fields, &values),
        values,
        deleted_at: None,
        created_by: created_by.map(str::to_string),
        created_at: now.clone(),
        updated_at: now,
    };
    conn.execute(
        "INSERT INTO records (id, object_id, title, data, deleted_at, created_by, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7)",
        params![
            record.id,
            record.object_id,
            record.title,
            encode_json(&record.values),
            record.created_by,
            record.created_at,
            record.updated_at
        ],
    )?;
    sync_links(conn, object, fields, &record.id, &record.values)?;
    fts_reindex(
        conn,
        &record.id,
        &record.title,
        &fts_body(fields, &record.values),
    )?;
    Ok(record)
}

/// Write a new value bag over an existing record, computing the diff, maintaining
/// links and FTS, and writing the automatic timeline entries.
pub(super) fn write_record_values(
    conn: &Connection,
    object: &Object,
    fields: &[Field],
    existing: &Record,
    next: ValueBag,
) -> Result<RecordUpdate> {
    let changed = diff_values(fields, &existing.values, &next);
    if changed.is_empty() {
        return Ok(RecordUpdate {
            record: existing.clone(),
            changed,
            stage_change: None,
        });
    }
    let now = now_rfc3339();
    let title = compute_title(object, fields, &next);
    conn.execute(
        "UPDATE records SET title = ?2, data = ?3, updated_at = ?4 WHERE id = ?1",
        params![existing.id, title, encode_json(&next), now],
    )?;
    sync_links(conn, object, fields, &existing.id, &next)?;
    fts_reindex(conn, &existing.id, &title, &fts_body(fields, &next))?;

    let stage_change = stage_change_from(fields, &changed);
    log_change_activities(conn, object, &existing.id, &changed, stage_change.as_ref())?;

    Ok(RecordUpdate {
        record: Record {
            title,
            values: next,
            updated_at: now,
            ..existing.clone()
        },
        changed,
        stage_change,
    })
}

/// Per-field before/after, in field position order so a timeline reads top-to-bottom
/// like the form does.
fn diff_values(fields: &[Field], before: &ValueBag, after: &ValueBag) -> Vec<FieldChange> {
    let mut changes = Vec::new();
    for field in fields {
        let from = before.get(&field.slug).cloned().unwrap_or(Value::Null);
        let to = after.get(&field.slug).cloned().unwrap_or(Value::Null);
        if from == to {
            continue;
        }
        changes.push(FieldChange {
            field_id: field.id.clone(),
            field_slug: field.slug.clone(),
            field_name: field.name.clone(),
            from,
            to,
        });
    }
    changes
}

/// Extract the first `status`-field transition from a diff, with both option ids and
/// both labels resolved.
fn stage_change_from(fields: &[Field], changes: &[FieldChange]) -> Option<StageChange> {
    for change in changes {
        let field = fields
            .iter()
            .find(|f| f.id == change.field_id && f.field_type == FieldType::Status)?;
        let label = |value: &Value| -> Option<String> {
            value
                .as_str()
                .and_then(|id| field.config.option(id))
                .map(|o| o.label.clone())
        };
        return Some(StageChange {
            field_id: field.id.clone(),
            field_slug: field.slug.clone(),
            from_label: label(&change.from),
            from: change.from.as_str().map(str::to_string),
            to_label: label(&change.to),
            to: change.to.as_str().map(str::to_string),
        });
    }
    None
}

/// Write the automatic `field_change` entry and, when a status field moved, the
/// `stage_change` entry the pipeline/funnel report reads.
fn log_change_activities(
    conn: &Connection,
    object: &Object,
    record_id: &str,
    changes: &[FieldChange],
    stage: Option<&StageChange>,
) -> Result<()> {
    let now = now_rfc3339();
    // ONE `field_change` per update, not one per field: a five-field save is one
    // edit, and five timeline rows for it is noise that buries the note above them.
    let summary = if changes.len() == 1 {
        format!("changed {}", changes[0].field_name)
    } else {
        format!("changed {} fields", changes.len())
    };
    conn.execute(
        "INSERT INTO activities
           (id, record_id, object_id, kind, title, body, field_id, from_value, to_value,
            assignee, due_at, completed_at, due_notified_at, author, metadata, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'field_change', ?4, NULL, ?5, ?6, ?7, NULL, NULL, NULL, NULL, NULL, ?8, ?9, ?9)",
        params![
            new_id(ID_ACTIVITY),
            record_id,
            object.id,
            summary,
            changes.first().map(|c| c.field_id.clone()),
            changes.first().map(|c| encode_json(&c.from)),
            changes.first().map(|c| encode_json(&c.to)),
            encode_json(&json!({ "changes": changes })),
            now
        ],
    )?;
    if let Some(stage) = stage {
        conn.execute(
            "INSERT INTO activities
               (id, record_id, object_id, kind, title, body, field_id, from_value, to_value,
                assignee, due_at, completed_at, due_notified_at, author, metadata, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'stage_change', ?4, NULL, ?5, ?6, ?7, NULL, NULL, NULL, NULL, NULL, NULL, ?8, ?8)",
            params![
                new_id(ID_ACTIVITY),
                record_id,
                object.id,
                format!(
                    "{} → {}",
                    stage.from_label.clone().unwrap_or_else(|| "—".to_string()),
                    stage.to_label.clone().unwrap_or_else(|| "—".to_string())
                ),
                stage.field_id,
                stage.from.as_ref().map(|v| encode_json(&json!(v))),
                stage.to.as_ref().map(|v| encode_json(&json!(v))),
                now
            ],
        )?;
    }
    Ok(())
}

// ── Relations ──────────────────────────────────────────────────────────────────

/// Bring the materialised edges for a record in line with its value bag.
///
/// The bag is authoritative and the edge table is a projection of it. Reconciling
/// (delete the gone, insert the new) rather than delete-all-then-reinsert keeps
/// `created_at` meaningful — "when did we link this company to this deal" is a
/// question people ask, and rewriting every edge on every unrelated save would
/// answer it with the time of the last edit to anything.
fn sync_links(
    conn: &Connection,
    object: &Object,
    fields: &[Field],
    record_id: &str,
    values: &ValueBag,
) -> Result<()> {
    let now = now_rfc3339();
    for field in fields
        .iter()
        .filter(|f| f.field_type == FieldType::Relation)
    {
        let Some(target_object) = field.config.relation_object_id.as_deref() else {
            continue;
        };
        let wanted: Vec<String> = values.get(&field.slug).map(as_list).unwrap_or_default();

        let existing: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT target_record_id FROM record_links WHERE field_id = ?1 AND source_record_id = ?2",
            )?;
            let rows = stmt.query_map(params![field.id, record_id], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        for gone in existing.iter().filter(|id| !wanted.contains(id)) {
            conn.execute(
                "DELETE FROM record_links WHERE field_id = ?1 AND source_record_id = ?2 AND target_record_id = ?3",
                params![field.id, record_id, gone],
            )?;
        }
        for added in wanted.iter().filter(|id| !existing.contains(id)) {
            conn.execute(
                "INSERT OR IGNORE INTO record_links
                   (id, field_id, source_record_id, source_object_id, target_record_id, target_object_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    new_id(ID_LINK),
                    field.id,
                    record_id,
                    object.id,
                    added,
                    target_object,
                    now
                ],
            )?;
        }
    }
    Ok(())
}

/// Every edge touching `record_id`, from that record's point of view, with the other
/// end's title resolved and the direction-appropriate label chosen.
pub(super) fn link_views(conn: &Connection, record_id: &str) -> Result<Vec<RecordLinkView>> {
    let sql = format!(
        "SELECT {COLS_LINK} FROM record_links
          WHERE source_record_id = ?1 OR target_record_id = ?1
          ORDER BY created_at ASC, id ASC"
    );
    let links = {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![record_id], row_to_link)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut out = Vec::with_capacity(links.len());
    for link in links {
        let outgoing = link.source_record_id == record_id;
        let other_id = if outgoing {
            &link.target_record_id
        } else {
            &link.source_record_id
        };
        let other_object = if outgoing {
            &link.target_object_id
        } else {
            &link.source_object_id
        };
        let title: Option<String> = conn
            .query_row(
                "SELECT title FROM records WHERE id = ?1 AND deleted_at IS NULL",
                params![other_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(title) = title else { continue };
        let field = load_field(conn, &link.field_id)?;
        let label = if outgoing {
            field
                .as_ref()
                .map(|f| f.name.clone())
                .unwrap_or_else(|| "Related".to_string())
        } else {
            // The inverse name, or the SOURCE object's plural. Without this the
            // company's page would label the edge with the person's field name and
            // read "Company: Jane Doe".
            field
                .as_ref()
                .and_then(|f| f.config.relation_inverse_label.clone())
                .or_else(|| {
                    load_object(conn, &link.source_object_id)
                        .ok()
                        .flatten()
                        .map(|o| o.plural)
                })
                .unwrap_or_else(|| "Related".to_string())
        };
        out.push(RecordLinkView {
            link_id: link.id,
            field_id: link.field_id,
            label,
            direction: if outgoing {
                LinkDirection::Outgoing
            } else {
                LinkDirection::Incoming
            },
            record_id: other_id.clone(),
            object_id: other_object.clone(),
            title,
            created_at: link.created_at,
        });
    }
    Ok(out)
}
