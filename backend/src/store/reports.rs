impl CrmStore {
    /// Counts and summed value per stage of a status field.
    pub async fn pipeline_report(&self, req: &PipelineRequest) -> Result<Option<PipelineReport>> {
        let conn = self.conn.lock().await;
        let object_ref = req.object_id.as_deref().unwrap_or("deal");
        let Some(object) = load_object(&conn, object_ref)? else {
            return Ok(None);
        };
        let fields = load_fields(&conn, &object.id)?;
        let index = field_index(&fields);
        let Some(stage_field) = pick_status_field(&fields, req.field_id.as_deref(), &index) else {
            return Ok(None);
        };
        let value_field = req
            .value_field_id
            .as_deref()
            .and_then(|id| index.get(id).cloned())
            .filter(|f| f.field_type == FieldType::Currency)
            .or_else(|| {
                fields
                    .iter()
                    .find(|f| f.field_type == FieldType::Currency)
                    .cloned()
            });

        let base = RecordQuery {
            object_id: object.id.clone(),
            filter: req.filter.clone(),
            ..Default::default()
        };
        let (base_where, base_params) = build_record_where(&base, &object.id, &index);

        let bucket = |option_id: Option<&str>| -> Result<(i64, i64)> {
            let mut params = base_params.clone();
            let clause = match option_id {
                Some(id) => {
                    params.push(rusqlite::types::Value::Text(id.to_string()));
                    format!("json_extract(r.data, '$.{}') = ?", stage_field.slug)
                }
                None => format!(
                    "(json_extract(r.data, '$.{slug}') IS NULL OR CAST(json_extract(r.data, '$.{slug}') AS TEXT) = '')",
                    slug = stage_field.slug
                ),
            };
            let where_sql = format!("{base_where} AND {clause}");
            let count: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM records r WHERE {where_sql}"),
                params_from_iter(params.clone()),
                |r| r.get(0),
            )?;
            let value: i64 = match value_field.as_ref() {
                Some(field) => conn
                    .query_row(
                        &format!(
                            "SELECT CAST(COALESCE(SUM(COALESCE(json_extract(r.data, '$.{}'), 0)), 0) AS INTEGER)
                               FROM records r WHERE {where_sql}",
                            field.slug
                        ),
                        params_from_iter(params),
                        |r| r.get(0),
                    )
                    .unwrap_or(0),
                None => 0,
            };
            Ok((count, value))
        };

        let options = stage_field.config.sorted_options();
        let mut stages = Vec::with_capacity(options.len());
        let mut total_records = 0i64;
        let mut total_value = 0i64;
        let (won_count, won_value, lost_count, lost_value) = {
            let mut w = (0i64, 0i64, 0i64, 0i64);
            for option in &options {
                if !req.include_closed && option.is_terminal() {
                    continue;
                }
                let (count, value) = bucket(Some(&option.id))?;
                total_records += count;
                total_value += value;
                if option.is_won {
                    w.0 += count;
                    w.1 += value;
                }
                if option.is_lost {
                    w.2 += count;
                    w.3 += value;
                }
                stages.push(PipelineStage {
                    option_id: option.id.clone(),
                    label: option.label.clone(),
                    color: option.color.clone(),
                    position: option.position,
                    is_won: option.is_won,
                    is_lost: option.is_lost,
                    record_count: count,
                    value_cents: value,
                    share: 0.0,
                });
            }
            w
        };
        // Counted, never dropped: a forecast that quietly excludes rows is wrong in
        // the direction nobody checks.
        let (unassigned_count, unassigned_value) = bucket(None)?;
        total_records += unassigned_count;
        total_value += unassigned_value;

        for stage in &mut stages {
            stage.share = if total_records > 0 {
                stage.record_count as f64 / total_records as f64
            } else {
                0.0
            };
        }
        let closed = won_count + lost_count;
        Ok(Some(PipelineReport {
            object_id: object.id,
            field_id: stage_field.id.clone(),
            currency_code: value_field
                .as_ref()
                .map(|f| f.config.currency().to_string())
                .unwrap_or_else(|| FieldConfig::DEFAULT_CURRENCY.to_string()),
            value_field_id: value_field.map(|f| f.id),
            total_records,
            total_value_cents: total_value,
            unassigned_count,
            stages,
            won_count,
            won_value_cents: won_value,
            lost_count,
            lost_value_cents: lost_value,
            win_rate: if closed > 0 {
                won_count as f64 / closed as f64
            } else {
                0.0
            },
        }))
    }

    /// Stage-to-stage conversion, reconstructed from the `stage_change` timeline.
    ///
    /// Computed IN MEMORY from one query rather than one aggregate per stage: the
    /// question "of the records that reached Proposal, how many went further" is a
    /// per-record path question, and expressing paths in SQL here would be a
    /// correlated subquery per stage per record.
    pub async fn funnel_report(&self, req: &FunnelRequest) -> Result<Option<FunnelReport>> {
        let conn = self.conn.lock().await;
        let object_ref = req.object_id.as_deref().unwrap_or("deal");
        let Some(object) = load_object(&conn, object_ref)? else {
            return Ok(None);
        };
        let fields = load_fields(&conn, &object.id)?;
        let index = field_index(&fields);
        let Some(stage_field) = pick_status_field(&fields, req.field_id.as_deref(), &index) else {
            return Ok(None);
        };
        let options = stage_field.config.sorted_options();
        let position_of: HashMap<&str, i64> = options
            .iter()
            .map(|o| (o.id.as_str(), o.position))
            .collect();

        // (record_id, option_id, entered_at) for every stage a record reached.
        let mut traces: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT record_id, from_value, to_value, created_at FROM activities
                  WHERE object_id = ?1 AND kind = 'stage_change' AND field_id = ?2
                    AND (?3 IS NULL OR created_at >= ?3) AND (?4 IS NULL OR created_at <= ?4)
                  ORDER BY record_id ASC, created_at ASC",
            )?;
            let rows = stmt.query_map(
                params![object.id, stage_field.id, req.since, req.until],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )?;
            for row in rows {
                let (record_id, from_raw, to_raw, at) = row?;
                let Some(record_id) = record_id else { continue };
                let entry = traces.entry(record_id).or_default();
                if entry.is_empty() {
                    // The stage the record was in before its first recorded move.
                    if let Some(from) = from_raw
                        .and_then(|v| serde_json::from_str::<Value>(&v).ok())
                        .and_then(|v| v.as_str().map(str::to_string))
                    {
                        entry.push((from, at.clone()));
                    }
                }
                if let Some(to) = to_raw
                    .and_then(|v| serde_json::from_str::<Value>(&v).ok())
                    .and_then(|v| v.as_str().map(str::to_string))
                {
                    entry.push((to, at));
                }
            }
        }
        // Records that never moved still ENTERED their current stage — created
        // straight into it. Omitting them makes a young pipeline look empty.
        {
            let sql = format!(
                "SELECT id, CAST(json_extract(data, '$.{}') AS TEXT), created_at FROM records
                  WHERE object_id = ?1 AND deleted_at IS NULL
                    AND (?2 IS NULL OR created_at >= ?2) AND (?3 IS NULL OR created_at <= ?3)",
                stage_field.slug
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![object.id, req.since, req.until], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (record_id, stage, created_at) = row?;
                let Some(stage) = stage.filter(|s| !s.is_empty()) else {
                    continue;
                };
                traces
                    .entry(record_id)
                    .or_insert_with(|| vec![(stage, created_at)]);
            }
        }

        let mut steps = Vec::with_capacity(options.len());
        for option in &options {
            let mut entered = 0i64;
            let mut advanced = 0i64;
            let mut durations: Vec<i64> = Vec::new();
            for trace in traces.values() {
                let Some(at_index) = trace.iter().position(|(id, _)| id == &option.id) else {
                    continue;
                };
                entered += 1;
                let reached_later = trace.iter().any(|(id, _)| {
                    position_of.get(id.as_str()).copied().unwrap_or(-1) > option.position
                });
                if reached_later {
                    advanced += 1;
                }
                if let (Some((_, from)), Some((_, to))) =
                    (trace.get(at_index), trace.get(at_index + 1))
                {
                    if let (Ok(a), Ok(b)) = (
                        chrono::DateTime::parse_from_rfc3339(from),
                        chrono::DateTime::parse_from_rfc3339(to),
                    ) {
                        durations.push((b - a).num_hours().max(0));
                    }
                }
            }
            steps.push(FunnelStep {
                option_id: option.id.clone(),
                label: option.label.clone(),
                position: option.position,
                is_won: option.is_won,
                is_lost: option.is_lost,
                entered,
                advanced,
                conversion_rate: if entered > 0 {
                    advanced as f64 / entered as f64
                } else {
                    0.0
                },
                avg_hours_in_stage: (!durations.is_empty())
                    .then(|| durations.iter().sum::<i64>() / durations.len() as i64),
            });
        }

        Ok(Some(FunnelReport {
            object_id: object.id,
            field_id: stage_field.id.clone(),
            since: req.since.clone(),
            until: req.until.clone(),
            steps,
        }))
    }

    /// The dock panel's header strip.
    pub async fn summary(&self, recent_limit: usize) -> Result<CrmSummary> {
        let objects = {
            let conn = self.conn.lock().await;
            object_summaries(&conn)?
        };
        let (total_records, open_tasks, overdue_tasks, recent_activity) = {
            let conn = self.conn.lock().await;
            let now = now_rfc3339();
            let total_records: i64 = conn.query_row(
                "SELECT COUNT(*) FROM records WHERE deleted_at IS NULL",
                [],
                |r| r.get(0),
            )?;
            let open_tasks: i64 = conn.query_row(
                "SELECT COUNT(*) FROM activities WHERE kind = 'task' AND completed_at IS NULL",
                [],
                |r| r.get(0),
            )?;
            let overdue_tasks: i64 = conn.query_row(
                "SELECT COUNT(*) FROM activities
                  WHERE kind = 'task' AND completed_at IS NULL AND due_at IS NOT NULL AND due_at <= ?1",
                params![now],
                |r| r.get(0),
            )?;
            let sql = format!(
                "SELECT {COLS_ACTIVITY} FROM activities ORDER BY created_at DESC, id DESC LIMIT ?1"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![recent_limit as i64], row_to_activity)?;
            (
                total_records,
                open_tasks,
                overdue_tasks,
                rows.collect::<rusqlite::Result<Vec<_>>>()?,
            )
        };
        // Taken after the lock is released, because `pipeline_report` takes it again
        // and this mutex is not reentrant.
        let pipeline = self
            .pipeline_report(&PipelineRequest {
                include_closed: true,
                ..Default::default()
            })
            .await?;
        Ok(CrmSummary {
            objects,
            total_records,
            open_tasks,
            overdue_tasks,
            recent_activity,
            pipeline,
        })
    }
}

/// The status field a report runs over: the named one, else the object's first by
/// position. `None` when the object has no status field at all.
fn pick_status_field(
    fields: &[Field],
    requested: Option<&str>,
    index: &HashMap<String, Field>,
) -> Option<Field> {
    requested
        .and_then(|id| index.get(id).cloned())
        .filter(|f| f.field_type == FieldType::Status)
        .or_else(|| {
            fields
                .iter()
                .find(|f| f.field_type == FieldType::Status)
                .cloned()
        })
}
