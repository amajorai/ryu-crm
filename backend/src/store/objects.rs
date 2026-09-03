impl CrmStore {
    pub async fn list_objects(&self) -> Result<Vec<Object>> {
        let conn = self.conn.lock().await;
        let sql =
            format!("SELECT {COLS_OBJECT} FROM objects ORDER BY position ASC, created_at ASC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_object)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Objects with the counts the sidebar renders.
    pub async fn list_object_summaries(&self) -> Result<Vec<ObjectSummary>> {
        let conn = self.conn.lock().await;
        object_summaries(&conn)
    }

    /// By id OR slug.
    pub async fn get_object(&self, id_or_slug: &str) -> Result<Option<Object>> {
        let conn = self.conn.lock().await;
        load_object(&conn, id_or_slug)
    }

    /// The whole schema in one lock: objects, their fields, their views, and every
    /// list. This is the panel's boot call.
    pub async fn schema(&self) -> Result<SchemaResponse> {
        let conn = self.conn.lock().await;
        let objects = {
            let sql =
                format!("SELECT {COLS_OBJECT} FROM objects ORDER BY position ASC, created_at ASC");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], row_to_object)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut out = Vec::with_capacity(objects.len());
        for object in objects {
            let fields = load_fields(&conn, &object.id)?;
            let views = load_views(&conn, &object.id)?;
            let record_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM records WHERE object_id = ?1 AND deleted_at IS NULL",
                params![object.id],
                |r| r.get(0),
            )?;
            out.push(ObjectWithFields {
                object,
                fields,
                views,
                record_count,
            });
        }
        let lists = {
            let sql =
                format!("SELECT {COLS_LIST} FROM lists ORDER BY position ASC, created_at ASC");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], row_to_list)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(SchemaResponse {
            objects: out,
            lists,
        })
    }

    /// Create a custom object.
    ///
    /// Also creates its `name` title field and an "All …" default table view, in the
    /// same transaction. A bare object with no fields and no view is unusable, and
    /// making the panel do three calls to reach a usable state is how you get objects
    /// stuck half-created.
    pub async fn create_object(&self, req: &CreateObjectRequest) -> Validated<Object> {
        let slug = req.slug.trim().to_lowercase();
        if !is_valid_slug(&slug) {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "slug",
                ValidationCode::Invalid,
                "an object slug must be lowercase letters, digits and underscores, and must not be a reserved word",
            )]));
        }
        let singular = req.singular.trim().to_string();
        if singular.is_empty() {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "singular",
                ValidationCode::Required,
                "a name is required",
            )]));
        }
        let plural = req
            .plural
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{singular}s"));

        let mut conn = self.conn.lock().await;
        if load_object(&conn, &slug)?.is_some() {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "slug",
                ValidationCode::NotUnique,
                format!("an object with the slug \"{slug}\" already exists"),
            )]));
        }
        let tx = conn.transaction()?;
        let now = now_rfc3339();
        let object_id = new_id(ID_OBJECT);
        let title_field_id = new_id(ID_FIELD);
        let position = next_position(
            &tx,
            "SELECT MAX(position) FROM objects WHERE ?1 IS NOT NULL",
            "x",
        )?;
        tx.execute(
            "INSERT INTO objects
               (id, slug, singular, plural, icon, description, title_field_id, is_standard, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, ?9)",
            params![
                object_id,
                slug,
                singular,
                plural,
                req.icon,
                req.description,
                title_field_id,
                position,
                now
            ],
        )?;
        tx.execute(
            "INSERT INTO fields
               (id, object_id, list_id, slug, name, field_type, config, description,
                is_required, is_unique, is_system, position, created_at, updated_at)
             VALUES (?1, ?2, NULL, 'name', 'Name', 'text', '{}', NULL, 1, 0, 1, 0, ?3, ?3)",
            params![title_field_id, object_id, now],
        )?;
        tx.execute(
            "INSERT INTO views
               (id, object_id, name, kind, filter, sorts, visible_fields, group_by_field_id,
                is_default, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'table', NULL, ?4, ?5, NULL, 1, 0, ?6, ?6)",
            params![
                new_id(ID_VIEW),
                object_id,
                format!("All {}", plural.to_lowercase()),
                encode_json(&vec![ViewSort::desc("updated_at")]),
                encode_json(&vec![title_field_id.clone()]),
                now
            ],
        )?;
        let object = load_object(&tx, &object_id)?.context("re-reading the object just created")?;
        tx.commit()?;
        Ok(Ok(object))
    }

    /// Rename / re-icon / re-title an object. Its `slug` is immutable.
    pub async fn update_object(
        &self,
        object_id: &str,
        req: &UpdateObjectRequest,
    ) -> Result<Option<Object>> {
        let conn = self.conn.lock().await;
        let Some(existing) = load_object(&conn, object_id)? else {
            return Ok(None);
        };
        let now = now_rfc3339();
        conn.execute(
            "UPDATE objects SET singular = ?2, plural = ?3, icon = ?4, description = ?5,
                                title_field_id = ?6, position = ?7, updated_at = ?8
             WHERE id = ?1",
            params![
                existing.id,
                req.singular
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&existing.singular),
                req.plural
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&existing.plural),
                req.icon.as_ref().or(existing.icon.as_ref()),
                req.description.as_ref().or(existing.description.as_ref()),
                req.title_field_id
                    .as_ref()
                    .or(existing.title_field_id.as_ref()),
                req.position.unwrap_or(existing.position),
                now
            ],
        )?;
        load_object(&conn, &existing.id)
    }

    /// Delete a custom object and everything under it.
    ///
    /// An explicit ordered cascade in ONE transaction (see the module docs for why
    /// not `ON DELETE CASCADE`). Refuses a standard object: `Ok(false)` when there
    /// was nothing to delete, `Err` when the caller asked for something the product
    /// does not permit — the two are different HTTP answers.
    pub async fn delete_object(&self, object_id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            return Ok(false);
        };
        if object.is_standard {
            bail!("the standard \"{}\" object cannot be deleted", object.slug);
        }
        let tx = conn.transaction()?;
        // FTS first: once the records are gone their rowids are unrecoverable.
        tx.execute(
            "DELETE FROM records_fts WHERE rowid IN (SELECT rowid FROM records WHERE object_id = ?1)",
            params![object.id],
        )?;
        tx.execute(
            "DELETE FROM record_links WHERE source_object_id = ?1 OR target_object_id = ?1",
            params![object.id],
        )?;
        tx.execute(
            "DELETE FROM list_entries WHERE list_id IN (SELECT id FROM lists WHERE object_id = ?1)",
            params![object.id],
        )?;
        tx.execute(
            "DELETE FROM activities WHERE object_id = ?1",
            params![object.id],
        )?;
        for table in ["records", "fields", "views", "lists", "import_jobs"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE object_id = ?1"),
                params![object.id],
            )?;
        }
        let n = tx.execute("DELETE FROM objects WHERE id = ?1", params![object.id])?;
        tx.commit()?;
        Ok(n > 0)
    }
}

pub(super) fn object_summaries(conn: &Connection) -> Result<Vec<ObjectSummary>> {
    let sql = format!("SELECT {COLS_OBJECT} FROM objects ORDER BY position ASC, created_at ASC");
    let objects = {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_object)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut out = Vec::with_capacity(objects.len());
    for object in objects {
        let field_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM fields WHERE object_id = ?1 AND list_id IS NULL",
            params![object.id],
            |r| r.get(0),
        )?;
        let record_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM records WHERE object_id = ?1 AND deleted_at IS NULL",
            params![object.id],
            |r| r.get(0),
        )?;
        let default_view_id: Option<String> = conn
            .query_row(
                "SELECT id FROM views WHERE object_id = ?1 AND is_default = 1 LIMIT 1",
                params![object.id],
                |r| r.get(0),
            )
            .optional()?;
        out.push(ObjectSummary {
            object,
            field_count,
            record_count,
            default_view_id,
        });
    }
    Ok(out)
}

// ── Fields ─────────────────────────────────────────────────────────────────────
