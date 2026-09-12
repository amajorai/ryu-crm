impl CrmStore {
    /// Upload a CSV: parse it, infer the columns, suggest a mapping. Writes nothing
    /// to `records`.
    pub async fn create_import(
        &self,
        req: &CreateImportRequest,
        max_bytes: usize,
    ) -> Validated<ImportJob> {
        if req.csv.len() > max_bytes {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "csv",
                ValidationCode::OutOfRange,
                format!("the file is larger than the {max_bytes}-byte import limit"),
            )]));
        }
        let delimiter = req
            .delimiter
            .as_deref()
            .and_then(|d| d.chars().next())
            .unwrap_or_else(|| sniff_delimiter(&req.csv));
        let rows = parse_csv(&req.csv, delimiter);
        if rows.is_empty() {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "csv",
                ValidationCode::Invalid,
                "that file has no rows",
            )]));
        }
        let has_header = req
            .has_header
            .unwrap_or_else(|| looks_like_header(&rows[0]));

        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, &req.object_id)? else {
            bail!("unknown object \"{}\"", req.object_id);
        };
        let fields = load_fields(&conn, &object.id)?;

        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        let data_rows = if has_header { &rows[1..] } else { &rows[..] };
        let mut columns = Vec::with_capacity(width);
        for index in 0..width {
            let name = if has_header {
                rows[0]
                    .get(index)
                    .map(|c| c.trim().to_string())
                    .filter(|c| !c.is_empty())
            } else {
                None
            }
            .unwrap_or_else(|| format!("Column {}", index + 1));
            let samples: Vec<String> = data_rows
                .iter()
                .filter_map(|r| r.get(index))
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .take(ImportColumn::SAMPLE_ROWS)
                .collect();
            columns.push(ImportColumn {
                index,
                suggested_field_id: suggest_field(&name, &fields).map(|f| f.id),
                name,
                samples,
            });
        }
        // Pre-fill the mapping from the suggestions: the commonest import is a file
        // exported from another CRM whose headers already match.
        let mappings: Vec<ImportMapping> = columns
            .iter()
            .map(|c| ImportMapping {
                column_index: c.index,
                field_id: c.suggested_field_id.clone(),
            })
            .collect();

        let now = now_rfc3339();
        let id = new_id(ID_IMPORT);
        conn.execute(
            "INSERT INTO import_jobs
               (id, object_id, filename, status, delimiter, has_header, row_count, columns,
                mappings, dedupe, preview, result, error, raw_csv, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'draft', ?4, ?5, ?6, ?7, ?8, '{}', NULL, NULL, NULL, ?9, ?10, ?10)",
            params![
                id,
                object.id,
                req.filename,
                delimiter.to_string(),
                i64::from(has_header),
                data_rows.len() as i64,
                encode_json(&columns),
                encode_json(&mappings),
                req.csv,
                now
            ],
        )?;
        let sql = format!("SELECT {COLS_IMPORT} FROM import_jobs WHERE id = ?1");
        Ok(Ok(conn.query_row(&sql, params![id], row_to_import)?))
    }

    pub async fn get_import(&self, import_id: &str) -> Result<Option<ImportJob>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_IMPORT} FROM import_jobs WHERE id = ?1");
        Ok(conn
            .query_row(&sql, params![import_id], row_to_import)
            .optional()?)
    }

    pub async fn list_imports(
        &self,
        object_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ImportJob>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {COLS_IMPORT} FROM import_jobs
              WHERE (?1 IS NULL OR object_id = ?1) ORDER BY created_at DESC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![object_id, limit as i64], row_to_import)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Save the column → field mapping and the dedupe rule. Clears any stale preview:
    /// a preview that describes a different mapping is worse than no preview.
    pub async fn set_import_mapping(
        &self,
        import_id: &str,
        req: &SetImportMappingRequest,
    ) -> Result<Option<ImportJob>> {
        let conn = self.conn.lock().await;
        let now = now_rfc3339();
        let n = conn.execute(
            "UPDATE import_jobs SET mappings = ?2, dedupe = ?3, preview = NULL, status = 'draft', updated_at = ?4
             WHERE id = ?1 AND status <> 'applied'",
            params![import_id, encode_json(&req.mappings), encode_json(&req.dedupe), now],
        )?;
        if n == 0 {
            return Ok(None);
        }
        let sql = format!("SELECT {COLS_IMPORT} FROM import_jobs WHERE id = ?1");
        Ok(conn
            .query_row(&sql, params![import_id], row_to_import)
            .optional()?)
    }

    pub async fn delete_import(&self, import_id: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn.execute("DELETE FROM import_jobs WHERE id = ?1", params![import_id])?;
        Ok(n > 0)
    }

    /// The dry run. Computes what every row WOULD do, writes nothing to `records`,
    /// and stores the report on the job.
    pub async fn dry_run_import(&self, import_id: &str) -> Result<Option<ImportPreview>> {
        let conn = self.conn.lock().await;
        let Some((job, raw, object, fields)) = load_import_context(&conn, import_id)? else {
            return Ok(None);
        };
        let (plans, conflicts) = plan_import(&conn, &job, &raw, &object, &fields)?;

        let mut preview = ImportPreview {
            total_rows: plans.len(),
            unmapped_columns: job
                .columns
                .iter()
                .filter(|c| {
                    job.mappings
                        .iter()
                        .find(|m| m.column_index == c.index)
                        .and_then(|m| m.field_id.as_deref())
                        .is_none()
                })
                .map(|c| c.name.clone())
                .collect(),
            ..Default::default()
        };
        for plan in &plans {
            match plan.action {
                ImportAction::Create => preview.create_count += 1,
                ImportAction::Update => preview.update_count += 1,
                ImportAction::Skip => preview.skip_count += 1,
                ImportAction::Error => preview.error_count += 1,
            }
        }
        // Errors first, then the head of the file: a preview that hides the failures
        // is worse than no preview.
        let mut samples: Vec<ImportRowPlan> = plans
            .iter()
            .filter(|p| p.action == ImportAction::Error)
            .take(ImportPreview::SAMPLE_LIMIT)
            .cloned()
            .collect();
        for plan in plans.iter().take(ImportPreview::SAMPLE_LIMIT) {
            if samples.len() >= ImportPreview::SAMPLE_LIMIT {
                break;
            }
            if !samples.iter().any(|s| s.row_index == plan.row_index) {
                samples.push(plan.clone());
            }
        }
        preview.truncated = plans.len() > samples.len();
        preview.samples = samples;
        preview.conflicts = conflicts
            .into_iter()
            .take(ImportPreview::CONFLICT_LIMIT)
            .collect();

        let now = now_rfc3339();
        conn.execute(
            "UPDATE import_jobs SET preview = ?2, status = 'previewed', updated_at = ?3 WHERE id = ?1",
            params![job.id, encode_json(&preview), now],
        )?;
        Ok(Some(preview))
    }

    /// Apply the mapping. ONE transaction over the whole file: a half-imported CSV is
    /// a reconciliation problem with no tooling, whereas a failed one is a retry.
    ///
    /// Returns the created/updated ids so the CALLER raises the `record.created` /
    /// `record.updated` events — the store never emits.
    pub async fn apply_import(&self, import_id: &str) -> Result<Option<ImportResult>> {
        let mut conn = self.conn.lock().await;
        let Some((job, raw, object, fields)) = load_import_context(&conn, import_id)? else {
            return Ok(None);
        };
        if job.status == ImportStatus::Applied {
            bail!("this import has already been applied");
        }
        let (plans, _) = plan_import(&conn, &job, &raw, &object, &fields)?;

        let tx = conn.transaction()?;
        let mut result = ImportResult::default();
        for plan in plans {
            match plan.action {
                ImportAction::Error => {
                    result.failed += 1;
                    if result.errors.len() < ImportResult::ERROR_LIMIT {
                        result.errors.push(ImportRowError {
                            row_index: plan.row_index,
                            errors: plan.errors,
                        });
                    }
                }
                ImportAction::Skip => result.skipped += 1,
                ImportAction::Create => {
                    let mut values = plan.values;
                    prune_nulls(&mut values);
                    let record = insert_record(
                        &tx,
                        &object,
                        &fields,
                        values,
                        Some(&format!("import:{}", job.id)),
                    )?;
                    result.created += 1;
                    result.created_record_ids.push(record.id);
                }
                ImportAction::Update => {
                    let Some(record_id) = plan.record_id else {
                        result.skipped += 1;
                        continue;
                    };
                    let Some(existing) = load_record(&tx, &record_id)? else {
                        result.skipped += 1;
                        continue;
                    };
                    let mut next = existing.values.clone();
                    for (slug, value) in plan.values {
                        if value.is_null() {
                            next.remove(&slug);
                        } else {
                            next.insert(slug, value);
                        }
                    }
                    prune_nulls(&mut next);
                    let update = write_record_values(&tx, &object, &fields, &existing, next)?;
                    if update.changed.is_empty() {
                        result.skipped += 1;
                    } else {
                        result.updated += 1;
                        result.updated_record_ids.push(record_id);
                    }
                }
            }
        }
        let now = now_rfc3339();
        tx.execute(
            "UPDATE import_jobs SET result = ?2, status = 'applied', updated_at = ?3 WHERE id = ?1",
            params![job.id, encode_json(&result), now],
        )?;
        tx.commit()?;
        Ok(Some(result))
    }
}

/// The job, its raw bytes, its object and its fields — everything both the dry run
/// and the apply need, loaded once so they cannot diverge.
#[allow(clippy::type_complexity)]
fn load_import_context(
    conn: &Connection,
    import_id: &str,
) -> Result<Option<(ImportJob, String, Object, Vec<Field>)>> {
    let sql = format!("SELECT {COLS_IMPORT}, raw_csv FROM import_jobs WHERE id = ?1");
    let row: Option<(ImportJob, String)> = conn
        .query_row(&sql, params![import_id], |row| {
            Ok((row_to_import(row)?, row.get::<_, String>(15)?))
        })
        .optional()?;
    let Some((job, raw)) = row else {
        return Ok(None);
    };
    let Some(object) = load_object(conn, &job.object_id)? else {
        return Ok(None);
    };
    let fields = load_fields(conn, &object.id)?;
    Ok(Some((job, raw, object, fields)))
}

/// The heart of the import: turn every data row into an [`ImportRowPlan`].
///
/// Shared verbatim by `dry_run_import` and `apply_import`, which is the whole point —
/// a preview computed by different code from the apply is a preview of nothing.
fn plan_import(
    conn: &Connection,
    job: &ImportJob,
    raw: &str,
    object: &Object,
    fields: &[Field],
) -> Result<(Vec<ImportRowPlan>, Vec<ImportConflict>)> {
    let index = field_index(fields);
    let delimiter = job.delimiter.chars().next().unwrap_or(',');
    let rows = parse_csv(raw, delimiter);
    let data_rows: &[Vec<String>] = if job.has_header && !rows.is_empty() {
        &rows[1..]
    } else {
        &rows[..]
    };

    // column index → field, resolved once.
    let mapping: Vec<(usize, Field)> = job
        .mappings
        .iter()
        .filter_map(|m| {
            let field_ref = m.field_id.as_deref()?;
            index.get(field_ref).map(|f| (m.column_index, f.clone()))
        })
        .collect();
    let match_fields: Vec<Field> = job
        .dedupe
        .match_field_ids
        .iter()
        .filter_map(|id| index.get(id.as_str()).cloned())
        .collect();

    let mut plans = Vec::with_capacity(data_rows.len());
    let mut conflicts = Vec::new();

    for (row_index, row) in data_rows.iter().enumerate() {
        let mut incoming = ValueBag::new();
        for (column_index, field) in &mapping {
            let Some(cell) = row.get(*column_index) else {
                continue;
            };
            if cell.trim().is_empty() {
                continue;
            }
            incoming.insert(field.slug.clone(), Value::String(cell.trim().to_string()));
        }

        // Find the existing record BEFORE validating, so uniqueness excludes it —
        // otherwise every `update` row would fail its own unique field.
        let matched = if match_fields.is_empty() {
            None
        } else {
            find_import_match(conn, &object.id, &match_fields, &incoming)?
        };

        let validated = validate_bag(
            conn,
            &object.id,
            fields,
            &incoming,
            matched.is_some(),
            matched.as_ref().map(|r| r.id.as_str()),
        )?;
        if !validated.is_ok() {
            plans.push(ImportRowPlan {
                row_index,
                action: ImportAction::Error,
                record_id: matched.map(|r| r.id),
                values: validated.values,
                errors: validated.errors,
            });
            continue;
        }
        let mut values = validated.values;

        let (action, record_id) = match (&matched, job.dedupe.strategy) {
            (None, _) | (Some(_), DedupeStrategy::CreateAlways) => (ImportAction::Create, None),
            (Some(existing), DedupeStrategy::Skip) => {
                (ImportAction::Skip, Some(existing.id.clone()))
            }
            (Some(existing), DedupeStrategy::Update) => {
                for (slug, incoming_value) in &values {
                    let current = existing.values.get(slug).cloned().unwrap_or(Value::Null);
                    if !is_empty_value(&current) && &current != incoming_value {
                        if let Some(field) = index.get(slug.as_str()) {
                            conflicts.push(ImportConflict {
                                row_index,
                                record_id: existing.id.clone(),
                                field_id: field.id.clone(),
                                field_slug: field.slug.clone(),
                                existing: current,
                                incoming: incoming_value.clone(),
                            });
                        }
                    }
                }
                (ImportAction::Update, Some(existing.id.clone()))
            }
            (Some(existing), DedupeStrategy::FillBlanks) => {
                values.retain(|slug, _| existing.values.get(slug).is_none_or(is_empty_value));
                if values.is_empty() {
                    (ImportAction::Skip, Some(existing.id.clone()))
                } else {
                    (ImportAction::Update, Some(existing.id.clone()))
                }
            }
        };

        // A create still has to satisfy the required fields, which the tolerant
        // `partial` pass above skipped for matched rows only.
        if action == ImportAction::Create {
            let missing: Vec<FieldValidationError> = fields
                .iter()
                .filter(|f| f.is_required)
                .filter(|f| values.get(&f.slug).is_none_or(is_empty_value))
                .map(|f| {
                    FieldValidationError::coded(
                        &f.id,
                        &f.slug,
                        ValidationCode::Required,
                        format!("{} is required", f.name),
                    )
                })
                .collect();
            if !missing.is_empty() {
                plans.push(ImportRowPlan {
                    row_index,
                    action: ImportAction::Error,
                    record_id: None,
                    values,
                    errors: missing,
                });
                continue;
            }
        }

        plans.push(ImportRowPlan {
            row_index,
            action,
            record_id,
            values,
            errors: Vec::new(),
        });
    }
    Ok((plans, conflicts))
}

/// Find the live record whose match fields ALL equal this row's, case- and
/// whitespace-insensitively. Rows missing any match value never match.
fn find_import_match(
    conn: &Connection,
    object_id: &str,
    match_fields: &[Field],
    incoming: &ValueBag,
) -> Result<Option<Record>> {
    let mut clauses = vec![
        "object_id = ?1".to_string(),
        "deleted_at IS NULL".to_string(),
    ];
    let mut params: SqlParams = vec![rusqlite::types::Value::Text(object_id.to_string())];
    for field in match_fields {
        let Some(text) = incoming.get(&field.slug).and_then(as_text) else {
            return Ok(None);
        };
        clauses.push(format!(
            "lower(trim(CAST(json_extract(data, '$.{}') AS TEXT))) = lower(trim(?))",
            field.slug
        ));
        params.push(rusqlite::types::Value::Text(text));
    }
    let sql = format!(
        "SELECT {COLS_RECORD} FROM records WHERE {} ORDER BY id ASC LIMIT 1",
        clauses.join(" AND ")
    );
    Ok(conn
        .query_row(&sql, params_from_iter(params), row_to_record)
        .optional()?)
}

/// Guess which field a CSV column belongs to: exact slug, then case-insensitive
/// name, then the slugified header.
fn suggest_field(header: &str, fields: &[Field]) -> Option<Field> {
    let trimmed = header.trim();
    let lowered = trimmed.to_lowercase();
    fields
        .iter()
        .find(|f| f.slug == lowered)
        .or_else(|| fields.iter().find(|f| f.name.eq_ignore_ascii_case(trimmed)))
        .or_else(|| {
            let slug = slugify(trimmed)?;
            fields.iter().find(|f| f.slug == slug)
        })
        .cloned()
}

// ── Search ─────────────────────────────────────────────────────────────────────
