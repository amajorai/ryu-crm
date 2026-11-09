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
            "records_terms MATCH crm_terms_query(?)".to_string(),
            "r.deleted_at IS NULL".to_string(),
            "crm_open('records.title', r.id, r.title) IS NOT NULL".to_string(),
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
                "SELECT COUNT(*) FROM records_terms JOIN records r ON r.rowid = records_terms.rowid WHERE {where_sql}"
            ),
            params_from_iter(params.clone()),
            |r| r.get(0),
        )?;
        // bm25 is ASCENDING-better; the client must not re-sort on it. The index
        // holds one blind token per distinct word (detail=none), so the rank comes
        // from how rare the matched words are, not how often a record repeats them.
        let sql = format!(
            "SELECT r.id, r.object_id, o.slug, crm_open('records.title', r.id, r.title),
                    crm_open('records.data', r.id, r.data), bm25(records_terms)
               FROM records_terms
               JOIN records r ON r.rowid = records_terms.rowid
               JOIN objects o ON o.id = r.object_id
              WHERE {where_sql}
              ORDER BY bm25(records_terms) ASC, r.updated_at DESC LIMIT ? OFFSET ?"
        );
        params.push(rusqlite::types::Value::Integer(limit as i64));
        params.push(rusqlite::types::Value::Integer(offset as i64));
        let rows: Vec<(SearchHit, Option<String>)> = {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(params), |row| {
                Ok((
                    SearchHit {
                        record_id: row.get(0)?,
                        object_id: row.get(1)?,
                        object_slug: row.get(2)?,
                        title: row.get(3)?,
                        snippet: String::new(),
                        rank: row.get(5)?,
                    },
                    row.get::<_, Option<String>>(4)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        // The index holds no text, so the snippet is cut from the decrypted
        // searchable fields, with the matched words marked.
        let mut fields_by_object: HashMap<String, Vec<Field>> = HashMap::new();
        let mut hits = Vec::with_capacity(rows.len());
        for (mut hit, data) in rows {
            if !fields_by_object.contains_key(&hit.object_id) {
                let fields = load_fields(&conn, &hit.object_id)?;
                fields_by_object.insert(hit.object_id.clone(), fields);
            }
            let values: ValueBag = data
                .and_then(|d| serde_json::from_str(&d).ok())
                .unwrap_or_default();
            let body = fts_body(&fields_by_object[&hit.object_id], &values);
            hit.snippet = crypto::make_snippet(&body, &query.query, 12);
            hits.push(hit);
        }
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
        tx.execute("DELETE FROM records_terms", [])?;
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
