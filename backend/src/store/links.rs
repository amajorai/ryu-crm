impl CrmStore {
    pub async fn list_links(&self, record_id: &str) -> Result<Vec<RecordLinkView>> {
        let conn = self.conn.lock().await;
        link_views(&conn, record_id)
    }

    /// Add relation targets. Writes the record's value bag — the bag is
    /// authoritative and `sync_links` projects it — so a link and an inline edit of
    /// the same field can never disagree.
    pub async fn link_records(
        &self,
        record_id: &str,
        req: &LinkRequest,
    ) -> Validated<Option<RecordUpdate>> {
        self.mutate_links(record_id, req, true).await
    }

    pub async fn unlink_records(
        &self,
        record_id: &str,
        req: &LinkRequest,
    ) -> Validated<Option<RecordUpdate>> {
        self.mutate_links(record_id, req, false).await
    }

    async fn mutate_links(
        &self,
        record_id: &str,
        req: &LinkRequest,
        add: bool,
    ) -> Validated<Option<RecordUpdate>> {
        let mut conn = self.conn.lock().await;
        let Some(existing) = load_record(&conn, record_id)? else {
            return Ok(Ok(None));
        };
        let Some(object) = load_object(&conn, &existing.object_id)? else {
            bail!("record {record_id} points at a missing object");
        };
        let fields = load_fields(&conn, &object.id)?;
        let Some(field) = fields
            .iter()
            .find(|f| f.id == req.field_id || f.slug == req.field_id)
            .filter(|f| f.field_type == FieldType::Relation)
            .cloned()
        else {
            return Ok(Err(vec![FieldValidationError::coded(
                &req.field_id,
                &req.field_id,
                ValidationCode::UnknownField,
                "not a relation field on this object",
            )]));
        };

        let mut targets: Vec<String> = existing
            .values
            .get(&field.slug)
            .map(as_list)
            .unwrap_or_default();
        for id in &req.target_record_ids {
            if add {
                if !targets.contains(id) {
                    targets.push(id.clone());
                }
            } else {
                targets.retain(|t| t != id);
            }
        }
        let mut patch = ValueBag::new();
        patch.insert(field.slug.clone(), json!(targets));
        let validated = validate_bag(&conn, &object.id, &fields, &patch, true, Some(&existing.id))?;
        if !validated.is_ok() {
            return Ok(Err(validated.errors));
        }
        let mut next = existing.values.clone();
        for (slug, value) in validated.values {
            if value.is_null() {
                next.remove(&slug);
            } else {
                next.insert(slug, value);
            }
        }
        prune_nulls(&mut next);

        let tx = conn.transaction()?;
        let update = write_record_values(&tx, &object, &fields, &existing, next)?;
        tx.commit()?;
        Ok(Ok(Some(update)))
    }

    /// Records on the other end of this record's edges.
    pub async fn related_records(
        &self,
        record_id: &str,
        query: &RelatedQuery,
        limit: usize,
        offset: usize,
    ) -> Result<RecordPage> {
        let conn = self.conn.lock().await;
        let mut views = link_views(&conn, record_id)?;
        if let Some(field_id) = query.field_id.as_deref().filter(|f| !f.is_empty()) {
            views.retain(|v| v.field_id == field_id);
        }
        let total = views.len() as i64;
        let ids: Vec<String> = views
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|v| v.record_id)
            .collect();
        let mut items = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(record) = load_record(&conn, &id)? {
                items.push(record);
            }
        }
        Ok(Page::new(items, total, limit, offset))
    }
}

// ── Dedupe + merge ─────────────────────────────────────────────────────────────
