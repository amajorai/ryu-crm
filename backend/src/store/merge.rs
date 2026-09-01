impl CrmStore {
    /// Records that share a normalized value on one of the match fields.
    ///
    /// With no fields named, the scan picks them: `is_unique` fields first, then
    /// email fields, then the title field. Returning WHICH fields it chose is part
    /// of the contract — a duplicate list the user cannot explain is a list they
    /// will not act on.
    pub async fn merge_candidates(
        &self,
        object_id: &str,
        req: &DuplicateScanRequest,
        limit: usize,
    ) -> Result<DuplicateScanResponse> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            return Ok(DuplicateScanResponse {
                candidates: Vec::new(),
                field_ids: Vec::new(),
            });
        };
        let fields = load_fields(&conn, &object.id)?;
        let index = field_index(&fields);
        let chosen: Vec<Field> = if req.field_ids.is_empty() {
            let unique: Vec<Field> = fields.iter().filter(|f| f.is_unique).cloned().collect();
            let emails: Vec<Field> = fields
                .iter()
                .filter(|f| f.field_type == FieldType::Email)
                .cloned()
                .collect();
            if !unique.is_empty() {
                unique
            } else if !emails.is_empty() {
                emails
            } else {
                object
                    .title_field_id
                    .as_deref()
                    .and_then(|id| index.get(id).cloned())
                    .into_iter()
                    .collect()
            }
        } else {
            req.field_ids
                .iter()
                .filter_map(|id| index.get(id.as_str()).cloned())
                .collect()
        };

        let mut candidates = Vec::new();
        for field in &chosen {
            let sql = format!(
                "SELECT lower(trim(CAST(json_extract(data, '$.{slug}') AS TEXT))) AS k,
                        group_concat(id, char(10)), COUNT(*)
                   FROM records
                  WHERE object_id = ?1 AND deleted_at IS NULL
                    AND json_extract(data, '$.{slug}') IS NOT NULL
                    AND trim(CAST(json_extract(data, '$.{slug}') AS TEXT)) <> ''
                  GROUP BY k HAVING COUNT(*) > 1
                  ORDER BY COUNT(*) DESC LIMIT ?2",
                slug = field.slug
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![object.id, limit as i64], |row| {
                let value: String = row.get(0)?;
                let ids: String = row.get(1)?;
                Ok((value, ids))
            })?;
            for row in rows {
                let (value, joined) = row?;
                // group_concat gives no ordering guarantee; sorting the ULID-ish ids
                // makes `record_ids[0]` deterministically the OLDEST, which is what
                // the UI suggests as the survivor.
                let mut record_ids: Vec<String> = joined
                    .split('\n')
                    .map(str::to_string)
                    .filter(|s| !s.is_empty())
                    .collect();
                record_ids.sort();
                let mut titles = Vec::with_capacity(record_ids.len());
                for id in &record_ids {
                    let title: Option<String> = conn
                        .query_row(
                            "SELECT title FROM records WHERE id = ?1",
                            params![id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    titles.push(title.unwrap_or_default());
                }
                candidates.push(MergeCandidate {
                    record_ids,
                    field_id: field.id.clone(),
                    field_slug: field.slug.clone(),
                    value,
                    score: 1.0,
                    titles,
                });
            }
        }
        candidates.truncate(limit);
        Ok(DuplicateScanResponse {
            candidates,
            field_ids: chosen.into_iter().map(|f| f.id).collect(),
        })
    }

    /// Dry run of a merge. Writes nothing.
    pub async fn plan_merge(&self, plan: &MergePlan) -> Result<Option<MergePreview>> {
        let conn = self.conn.lock().await;
        let Some((survivor, losers, fields, resolved, conflicts)) = resolve_merge(&conn, plan)?
        else {
            return Ok(None);
        };
        let _ = fields;
        let mut activity_count = 0i64;
        let mut link_count = 0i64;
        let mut list_entry_count = 0i64;
        for loser in &losers {
            activity_count += conn.query_row(
                "SELECT COUNT(*) FROM activities WHERE record_id = ?1",
                params![loser.id],
                |r| r.get::<_, i64>(0),
            )?;
            link_count += conn.query_row(
                "SELECT COUNT(*) FROM record_links WHERE source_record_id = ?1 OR target_record_id = ?1",
                params![loser.id],
                |r| r.get::<_, i64>(0),
            )?;
            list_entry_count += conn.query_row(
                "SELECT COUNT(*) FROM list_entries WHERE record_id = ?1",
                params![loser.id],
                |r| r.get::<_, i64>(0),
            )?;
        }
        Ok(Some(MergePreview {
            survivor,
            losers,
            resolved_values: resolved,
            conflicts,
            activity_count,
            link_count,
            list_entry_count,
        }))
    }

    /// Perform the merge: resolve values onto the survivor, move history, links and
    /// list memberships, then retire the losers. One transaction — a half-merged
    /// pair is worse than either outcome.
    pub async fn merge_records(&self, plan: &MergePlan) -> Result<Option<MergeOutcome>> {
        let mut conn = self.conn.lock().await;
        let Some((survivor, losers, fields, resolved, _)) = resolve_merge(&conn, plan)? else {
            return Ok(None);
        };
        let Some(object) = load_object(&conn, &survivor.object_id)? else {
            bail!("record {} points at a missing object", survivor.id);
        };
        let now = now_rfc3339();
        let tx = conn.transaction()?;

        let mut moved_activities = 0i64;
        let mut moved_links = 0i64;
        let mut moved_list_entries = 0i64;
        for loser in &losers {
            moved_activities += tx.execute(
                "UPDATE activities SET record_id = ?2, updated_at = ?3 WHERE record_id = ?1",
                params![loser.id, survivor.id, now],
            )? as i64;
            // `INSERT OR IGNORE`-style repointing: the unique edge index rejects a
            // duplicate, so re-point what can move and drop what would collide.
            moved_links += tx.execute(
                "UPDATE OR IGNORE record_links SET source_record_id = ?2 WHERE source_record_id = ?1",
                params![loser.id, survivor.id],
            )? as i64;
            moved_links += tx.execute(
                "UPDATE OR IGNORE record_links SET target_record_id = ?2 WHERE target_record_id = ?1",
                params![loser.id, survivor.id],
            )? as i64;
            tx.execute(
                "DELETE FROM record_links WHERE source_record_id = ?1 OR target_record_id = ?1",
                params![loser.id],
            )?;
            moved_list_entries += tx.execute(
                "UPDATE OR IGNORE list_entries SET record_id = ?2, updated_at = ?3 WHERE record_id = ?1",
                params![loser.id, survivor.id, now],
            )? as i64;
            tx.execute(
                "DELETE FROM list_entries WHERE record_id = ?1",
                params![loser.id],
            )?;

            if plan.soft_delete_losers {
                tx.execute(
                    "UPDATE records SET deleted_at = ?2, updated_at = ?2 WHERE id = ?1",
                    params![loser.id, now],
                )?;
            } else {
                fts_delete(&tx, &loser.id)?;
                tx.execute("DELETE FROM records WHERE id = ?1", params![loser.id])?;
            }
        }

        let update = write_record_values(&tx, &object, &fields, &survivor, resolved)?;
        // A dedicated timeline entry, because "why does this record now say what the
        // other one said" is the first question after any merge.
        tx.execute(
            "INSERT INTO activities
               (id, record_id, object_id, kind, title, body, field_id, from_value, to_value,
                assignee, due_at, completed_at, due_notified_at, author, metadata, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'note', ?4, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, ?5, ?6, ?6)",
            params![
                new_id(ID_ACTIVITY),
                survivor.id,
                object.id,
                format!("Merged {} record(s) into this one", losers.len()),
                encode_json(&json!({ "merged_record_ids": losers.iter().map(|l| l.id.clone()).collect::<Vec<_>>() })),
                now
            ],
        )?;
        tx.commit()?;

        Ok(Some(MergeOutcome {
            survivor: update.record,
            merged_record_ids: losers.into_iter().map(|l| l.id).collect(),
            moved_activities,
            moved_links,
            moved_list_entries,
            changed: update.changed,
        }))
    }
}

/// Shared by the preview and the apply, so the dry run cannot describe a different
/// merge from the one that happens.
#[allow(clippy::type_complexity)]
fn resolve_merge(
    conn: &Connection,
    plan: &MergePlan,
) -> Result<
    Option<(
        Record,
        Vec<Record>,
        Vec<Field>,
        ValueBag,
        Vec<MergeConflict>,
    )>,
> {
    let Some(survivor) = load_record(conn, &plan.survivor_id)? else {
        return Ok(None);
    };
    let mut losers = Vec::new();
    for id in &plan.loser_ids {
        if id == &survivor.id {
            bail!("a record cannot be merged into itself");
        }
        let Some(loser) = load_record(conn, id)? else {
            continue;
        };
        if loser.object_id != survivor.object_id {
            bail!("records on different objects cannot be merged");
        }
        losers.push(loser);
    }
    if losers.is_empty() {
        bail!("a merge needs at least one record to merge in");
    }
    let fields = load_fields(conn, &survivor.object_id)?;
    let explicit: HashMap<&str, &MergeSource> = plan
        .resolutions
        .iter()
        .map(|r| (r.field_id.as_str(), &r.source))
        .collect();

    let mut resolved = survivor.values.clone();
    let mut conflicts = Vec::new();
    for field in &fields {
        let survivor_value = survivor
            .values
            .get(&field.slug)
            .cloned()
            .unwrap_or(Value::Null);
        let differing: Vec<MergeLoserValue> = losers
            .iter()
            .filter_map(|loser| {
                let value = loser
                    .values
                    .get(&field.slug)
                    .cloned()
                    .unwrap_or(Value::Null);
                (!is_empty_value(&value) && value != survivor_value).then(|| MergeLoserValue {
                    record_id: loser.id.clone(),
                    title: loser.title.clone(),
                    value,
                })
            })
            .collect();

        let source = explicit
            .get(field.id.as_str())
            .or_else(|| explicit.get(field.slug.as_str()));
        let chosen = match source {
            Some(MergeSource::Survivor) => survivor_value.clone(),
            Some(MergeSource::Loser { record_id }) => losers
                .iter()
                .find(|l| &l.id == record_id)
                .and_then(|l| l.values.get(&field.slug).cloned())
                .unwrap_or(Value::Null),
            Some(MergeSource::Value { value }) => match validate_field_value(field, value) {
                Ok(v) => v.unwrap_or(Value::Null),
                Err(_) => survivor_value.clone(),
            },
            // The default: keep what the survivor has, and only FILL A BLANK from a
            // loser. Never silently overwrite — that is the behaviour every CRM gets
            // complained about.
            None => {
                if is_empty_value(&survivor_value) {
                    differing
                        .first()
                        .map(|l| l.value.clone())
                        .unwrap_or(Value::Null)
                } else {
                    survivor_value.clone()
                }
            }
        };

        if !differing.is_empty() && !is_empty_value(&survivor_value) {
            conflicts.push(MergeConflict {
                field_id: field.id.clone(),
                field_slug: field.slug.clone(),
                field_name: field.name.clone(),
                survivor_value: survivor_value.clone(),
                loser_values: differing,
            });
        }
        if is_empty_value(&chosen) {
            resolved.remove(&field.slug);
        } else {
            resolved.insert(field.slug.clone(), chosen);
        }
    }
    Ok(Some((survivor, losers, fields, resolved, conflicts)))
}

// ── Views ──────────────────────────────────────────────────────────────────────

pub(super) fn load_views(conn: &Connection, object_id: &str) -> Result<Vec<View>> {
    let sql = format!(
        "SELECT {COLS_VIEW} FROM views WHERE object_id = ?1 ORDER BY position ASC, created_at ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![object_id], row_to_view)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(super) fn load_view(conn: &Connection, view_id: &str) -> Result<Option<View>> {
    let sql = format!("SELECT {COLS_VIEW} FROM views WHERE id = ?1");
    Ok(conn
        .query_row(&sql, params![view_id], row_to_view)
        .optional()?)
}
