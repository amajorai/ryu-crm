impl CrmStore {
    pub async fn get_activity(&self, activity_id: &str) -> Result<Option<Activity>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_ACTIVITY} FROM activities WHERE id = ?1");
        Ok(conn
            .query_row(&sql, params![activity_id], row_to_activity)
            .optional()?)
    }

    pub async fn query_activities(
        &self,
        query: &ActivityQuery,
        limit: usize,
        offset: usize,
    ) -> Result<ActivityPage> {
        let conn = self.conn.lock().await;
        let mut clauses = vec!["1".to_string()];
        let mut params: SqlParams = Vec::new();
        if let Some(record_id) = query.record_id.as_deref().filter(|r| !r.is_empty()) {
            clauses.push("a.record_id = ?".to_string());
            params.push(rusqlite::types::Value::Text(record_id.to_string()));
        }
        if let Some(object_ref) = query.object_id.as_deref().filter(|o| !o.is_empty()) {
            let Some(object) = load_object(&conn, object_ref)? else {
                return Ok(Page::empty(limit, offset));
            };
            clauses.push("a.object_id = ?".to_string());
            params.push(rusqlite::types::Value::Text(object.id));
        }
        if !query.kinds.is_empty() {
            let placeholders = query
                .kinds
                .iter()
                .map(|k| {
                    params.push(rusqlite::types::Value::Text(k.as_str().to_string()));
                    "?"
                })
                .collect::<Vec<_>>()
                .join(", ");
            clauses.push(format!("a.kind IN ({placeholders})"));
        }
        if let Some(assignee) = query.assignee.as_deref().filter(|a| !a.is_empty()) {
            clauses.push("a.assignee = ?".to_string());
            params.push(rusqlite::types::Value::Text(assignee.to_string()));
        }
        if let Some(search) = query.search.as_deref().filter(|s| !s.trim().is_empty()) {
            clauses.push("(lower(a.title) LIKE '%' || lower(?) || '%' OR lower(COALESCE(a.body, '')) LIKE '%' || lower(?) || '%')".to_string());
            params.push(rusqlite::types::Value::Text(search.to_string()));
            params.push(rusqlite::types::Value::Text(search.to_string()));
        }
        // Fixed-width RFC-3339 makes these correct as TEXT range scans.
        if let Some(since) = query.since.as_deref().filter(|s| !s.is_empty()) {
            clauses.push("a.created_at >= ?".to_string());
            params.push(rusqlite::types::Value::Text(since.to_string()));
        }
        if let Some(until) = query.until.as_deref().filter(|s| !s.is_empty()) {
            clauses.push("a.created_at <= ?".to_string());
            params.push(rusqlite::types::Value::Text(until.to_string()));
        }
        let where_sql = clauses.join(" AND ");
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM activities a WHERE {where_sql}"),
            params_from_iter(params.clone()),
            |r| r.get(0),
        )?;
        let cols = COLS_ACTIVITY
            .split(", ")
            .map(|c| format!("a.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {cols} FROM activities a WHERE {where_sql}
              ORDER BY a.created_at DESC, a.id DESC LIMIT ? OFFSET ?"
        );
        params.push(rusqlite::types::Value::Integer(limit as i64));
        params.push(rusqlite::types::Value::Integer(offset as i64));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params), row_to_activity)?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page::new(items, total, limit, offset))
    }

    /// Create a user-authored timeline entry.
    ///
    /// Refuses the two automatic kinds: a hand-forged `field_change` is an audit
    /// trail that lies, which is worse than not having one.
    pub async fn create_activity(&self, req: &CreateActivityRequest) -> Validated<Activity> {
        if !req.kind.is_user_authored() {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "kind",
                ValidationCode::Invalid,
                format!(
                    "\"{}\" entries are written automatically and cannot be created directly",
                    req.kind.as_str()
                ),
            )]));
        }
        let conn = self.conn.lock().await;
        let mut object_id: Option<String> = None;
        if let Some(record_id) = req.record_id.as_deref().filter(|r| !r.is_empty()) {
            let Some(record) = load_record(&conn, record_id)? else {
                return Ok(Err(vec![FieldValidationError::coded(
                    "",
                    "record_id",
                    ValidationCode::BadRelationTarget,
                    "no such record",
                )]));
            };
            object_id = Some(record.object_id);
        }
        let due_at = match req.due_at.as_deref().filter(|d| !d.trim().is_empty()) {
            Some(raw) => match normalize_datetime(raw) {
                Some(normalized) => Some(normalized),
                None => {
                    return Ok(Err(vec![FieldValidationError::coded(
                        "",
                        "due_at",
                        ValidationCode::Invalid,
                        "not a valid date and time",
                    )]))
                }
            },
            None => None,
        };
        let now = now_rfc3339();
        let id = new_id(ID_ACTIVITY);
        conn.execute(
            "INSERT INTO activities
               (id, record_id, object_id, kind, title, body, field_id, from_value, to_value,
                assignee, due_at, completed_at, due_notified_at, author, metadata, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL, ?7, ?8, NULL, NULL, ?9, ?10, ?11, ?11)",
            params![
                id,
                req.record_id,
                object_id,
                req.kind.as_str(),
                req.title.trim(),
                req.body,
                req.assignee,
                due_at,
                req.author,
                req.metadata.as_ref().map(encode_json),
                now
            ],
        )?;
        let sql = format!("SELECT {COLS_ACTIVITY} FROM activities WHERE id = ?1");
        let activity = conn.query_row(&sql, params![id], row_to_activity)?;
        Ok(Ok(activity))
    }

    pub async fn update_activity(
        &self,
        activity_id: &str,
        req: &UpdateActivityRequest,
    ) -> Validated<Option<Activity>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_ACTIVITY} FROM activities WHERE id = ?1");
        let Some(existing) = conn
            .query_row(&sql, params![activity_id], row_to_activity)
            .optional()?
        else {
            return Ok(Ok(None));
        };
        let due_at = match req.due_at.as_deref() {
            Some(raw) if raw.trim().is_empty() => None,
            Some(raw) => match normalize_datetime(raw) {
                Some(normalized) => Some(normalized),
                None => {
                    return Ok(Err(vec![FieldValidationError::coded(
                        "",
                        "due_at",
                        ValidationCode::Invalid,
                        "not a valid date and time",
                    )]))
                }
            },
            None => existing.due_at.clone(),
        };
        let now = now_rfc3339();
        let completed_at = match req.completed {
            Some(true) => Some(existing.completed_at.clone().unwrap_or_else(|| now.clone())),
            Some(false) => None,
            None => existing.completed_at.clone(),
        };
        conn.execute(
            "UPDATE activities SET title = ?2, body = ?3, assignee = ?4, due_at = ?5,
                                   completed_at = ?6, metadata = ?7, updated_at = ?8
             WHERE id = ?1",
            params![
                existing.id,
                req.title
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or(&existing.title),
                req.body.as_ref().or(existing.body.as_ref()),
                req.assignee.as_ref().or(existing.assignee.as_ref()),
                due_at,
                completed_at,
                req.metadata
                    .as_ref()
                    .map(encode_json)
                    .or_else(|| existing.metadata.as_ref().map(encode_json)),
                now
            ],
        )?;
        Ok(Ok(conn
            .query_row(&sql, params![existing.id], row_to_activity)
            .optional()?))
    }

    pub async fn delete_activity(&self, activity_id: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn.execute("DELETE FROM activities WHERE id = ?1", params![activity_id])?;
        Ok(n > 0)
    }

    /// Complete or reopen a task.
    pub async fn complete_task(
        &self,
        activity_id: &str,
        completed: bool,
    ) -> Result<Option<Activity>> {
        let conn = self.conn.lock().await;
        let now = now_rfc3339();
        conn.execute(
            "UPDATE activities SET completed_at = ?2, updated_at = ?3 WHERE id = ?1 AND kind = 'task'",
            params![activity_id, completed.then(|| now.clone()), now],
        )?;
        let sql = format!("SELECT {COLS_ACTIVITY} FROM activities WHERE id = ?1");
        Ok(conn
            .query_row(&sql, params![activity_id], row_to_activity)
            .optional()?)
    }

    pub async fn list_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        offset: usize,
    ) -> Result<ActivityPage> {
        let conn = self.conn.lock().await;
        let now = now_rfc3339();
        let mut clauses = vec!["a.kind = 'task'".to_string()];
        let mut params: SqlParams = Vec::new();
        match query.filter {
            TaskFilter::Open => clauses.push("a.completed_at IS NULL".to_string()),
            TaskFilter::Completed => clauses.push("a.completed_at IS NOT NULL".to_string()),
            TaskFilter::Overdue => {
                clauses.push(
                    "a.completed_at IS NULL AND a.due_at IS NOT NULL AND a.due_at <= ?".to_string(),
                );
                params.push(rusqlite::types::Value::Text(now.clone()));
            }
            TaskFilter::All => {}
        }
        if let Some(assignee) = query.assignee.as_deref().filter(|a| !a.is_empty()) {
            clauses.push("a.assignee = ?".to_string());
            params.push(rusqlite::types::Value::Text(assignee.to_string()));
        }
        if let Some(record_id) = query.record_id.as_deref().filter(|r| !r.is_empty()) {
            clauses.push("a.record_id = ?".to_string());
            params.push(rusqlite::types::Value::Text(record_id.to_string()));
        }
        if let Some(object_ref) = query.object_id.as_deref().filter(|o| !o.is_empty()) {
            let Some(object) = load_object(&conn, object_ref)? else {
                return Ok(Page::empty(limit, offset));
            };
            clauses.push("a.object_id = ?".to_string());
            params.push(rusqlite::types::Value::Text(object.id));
        }
        if let Some(before) = query.due_before.as_deref().filter(|d| !d.is_empty()) {
            clauses.push("a.due_at <= ?".to_string());
            params.push(rusqlite::types::Value::Text(before.to_string()));
        }
        if let Some(after) = query.due_after.as_deref().filter(|d| !d.is_empty()) {
            clauses.push("a.due_at >= ?".to_string());
            params.push(rusqlite::types::Value::Text(after.to_string()));
        }
        let where_sql = clauses.join(" AND ");
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM activities a WHERE {where_sql}"),
            params_from_iter(params.clone()),
            |r| r.get(0),
        )?;
        let cols = COLS_ACTIVITY
            .split(", ")
            .map(|c| format!("a.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        // Undated tasks last: a task with a due date is the one that needs doing.
        let sql = format!(
            "SELECT {cols} FROM activities a WHERE {where_sql}
              ORDER BY (a.due_at IS NULL) ASC, a.due_at ASC, a.created_at DESC LIMIT ? OFFSET ?"
        );
        params.push(rusqlite::types::Value::Integer(limit as i64));
        params.push(rusqlite::types::Value::Integer(offset as i64));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params), row_to_activity)?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page::new(items, total, limit, offset))
    }

    /// Claim overdue tasks for the `task.due` sweep.
    ///
    /// The claim and the selection are ONE statement: `UPDATE … WHERE id IN (SELECT
    /// …) RETURNING …` stamps `due_notified_at` on exactly the rows it hands back, so
    /// a crash between "found it" and "announced it" cannot re-announce, and two
    /// sweeps cannot both claim the same task. A read-then-blind-update pair has no
    /// claim semantics at all — both callers would "win".
    pub async fn claim_due_tasks(&self, limit: usize) -> Result<Vec<Activity>> {
        let conn = self.conn.lock().await;
        let now = now_rfc3339();
        let sql = format!(
            "UPDATE activities SET due_notified_at = ?1, updated_at = ?1
              WHERE id IN (
                SELECT id FROM activities
                 WHERE kind = 'task' AND completed_at IS NULL AND due_notified_at IS NULL
                   AND due_at IS NOT NULL AND due_at <= ?1
                 ORDER BY due_at ASC LIMIT ?2)
            RETURNING {COLS_ACTIVITY}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![now, limit as i64], row_to_activity)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

// ── CSV import ─────────────────────────────────────────────────────────────────

/// RFC-4180 CSV, hand-rolled.
///
/// A dependency would churn the shared `Cargo.lock` for every other job building
/// this tree, and the grammar that actually matters is small: quoted fields, `""`
/// as an escaped quote, embedded newlines inside quotes, and `\r\n` normalised to
/// `\n`. Rows are NOT padded or truncated to a common width here — a short row is a
/// real signal the mapper needs to see.
pub fn parse_csv(raw: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = raw.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(ch);
            }
            continue;
        }
        match ch {
            '"' if field.is_empty() => in_quotes = true,
            c if c == delimiter => row.push(std::mem::take(&mut field)),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    // A trailing newline produces one empty row; drop rows that are entirely blank.
    rows.retain(|r| r.iter().any(|c| !c.trim().is_empty()));
    rows
}

/// Pick the delimiter by counting candidates on the first line. Comma-first, because
/// a tie on a single-column file should read as CSV.
pub fn sniff_delimiter(raw: &str) -> char {
    let first = raw.lines().next().unwrap_or_default();
    [',', ';', '\t', '|']
        .into_iter()
        .max_by_key(|d| first.matches(*d).count())
        .filter(|d| first.contains(*d))
        .unwrap_or(',')
}

/// A first row is a header when every cell is non-empty, non-numeric and distinct.
/// Getting this wrong in either direction is recoverable — the caller can override
/// `has_header` — but guessing well is what makes the common case one click.
pub(super) fn looks_like_header(row: &[String]) -> bool {
    let mut seen = HashSet::new();
    row.iter().all(|cell| {
        let trimmed = cell.trim();
        !trimmed.is_empty()
            && trimmed.parse::<f64>().is_err()
            && seen.insert(trimmed.to_lowercase())
    })
}
