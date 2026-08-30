impl CrmStore {
    /// Full-text search across every object's records.
    pub async fn search(
        &self,
        query: &SearchQuery,
        limit: usize,
        offset: usize,
    ) -> Result<SearchResponse> {
        let conn = self.conn.lock().await;
        let Some(expression) = fts_match_expression(&query.query) else {
            return Ok(SearchResponse {
                query: query.query.clone(),
                hits: Vec::new(),
                total: 0,
                limit,
                offset,
            });
        };
        let mut clauses = vec![
            "records_fts MATCH ?".to_string(),
            "r.deleted_at IS NULL".to_string(),
        ];
        let mut params: SqlParams = vec![rusqlite::types::Value::Text(expression)];
        if !query.object_ids.is_empty() {
            let mut ids = Vec::new();
            for object_ref in &query.object_ids {
                if let Some(object) = load_object(&conn, object_ref)? {
                    ids.push(object.id);
                }
            }
            if ids.is_empty() {
                return Ok(SearchResponse {
                    query: query.query.clone(),
                    hits: Vec::new(),
                    total: 0,
                    limit,
                    offset,
                });
            }
            let placeholders = ids
                .iter()
                .map(|id| {
                    params.push(rusqlite::types::Value::Text(id.clone()));
                    "?"
                })
                .collect::<Vec<_>>()
                .join(", ");
            clauses.push(format!("r.object_id IN ({placeholders})"));
        }
        let where_sql = clauses.join(" AND ");
        let total: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM records_fts JOIN records r ON r.rowid = records_fts.rowid WHERE {where_sql}"
            ),
            params_from_iter(params.clone()),
            |r| r.get(0),
        )?;
        // bm25 is ASCENDING-better; the client must not re-sort on it.
        let sql = format!(
            "SELECT r.id, r.object_id, o.slug, r.title,
                    snippet(records_fts, 1, '<mark>', '</mark>', '…', 12), bm25(records_fts)
               FROM records_fts
               JOIN records r ON r.rowid = records_fts.rowid
               JOIN objects o ON o.id = r.object_id
              WHERE {where_sql}
              ORDER BY bm25(records_fts) ASC, r.updated_at DESC LIMIT ? OFFSET ?"
        );
        params.push(rusqlite::types::Value::Integer(limit as i64));
        params.push(rusqlite::types::Value::Integer(offset as i64));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params), |row| {
            Ok(SearchHit {
                record_id: row.get(0)?,
                object_id: row.get(1)?,
                object_slug: row.get(2)?,
                title: row.get(3)?,
                snippet: row.get(4)?,
                rank: row.get(5)?,
            })
        })?;
        let hits = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(SearchResponse {
            query: query.query.clone(),
            hits,
            total,
            limit,
            offset,
        })
    }

    /// Rebuild the whole FTS index. The repair hatch for a database restored from a
    /// backup taken mid-write, and the only way to pick up a change to what
    /// [`FieldType::is_searchable`] returns.
    pub async fn reindex_all(&self) -> Result<usize> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM records_fts", [])?;
        let object_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM objects")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut total = 0;
        for object_id in object_ids {
            total += reindex_object(&tx, &object_id)?;
        }
        tx.commit()?;
        Ok(total)
    }
}

// ── Reports ────────────────────────────────────────────────────────────────────
