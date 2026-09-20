// ── Value validation ───────────────────────────────────────────────────────────
//
// TWO LAYERS, and downstream code needs to know which is which:
//
//   * [`validate_field_value`] is PURE — type/shape/range normalization only. It
//     turns "$1,234.56" into 123456 cents, "Proposal" into `opt_deal_stage_proposal`,
//     "yes" into `true`, "31/03/2026"-shaped input into `2026-03-31`. It cannot
//     check that a relation target exists or that a unique value is free, because it
//     has no database.
//   * [`validate_bag`] runs that over a whole value bag AND adds the two checks
//     that need the connection: relation-target existence and uniqueness. It also
//     enforces `is_required` (for a non-partial write).
//
// The import path calls both, per row. The merge path calls the pure layer on any
// value a user typed into the resolution dialog.

/// Read a JSON value as a trimmed string, coercing numbers and booleans. `None` for
/// null, an empty/blank string, or a container.
pub(super) fn as_text(raw: &Value) -> Option<String> {
    match raw {
        Value::Null => None,
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Read a JSON value as an f64, tolerating a formatted string ("1,234.56", "$1 200",
/// "45%"). This is the CSV path: a spreadsheet exports money with separators.
pub(super) fn as_number(raw: &Value) -> Option<f64> {
    match raw {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => {
            let cleaned: String = s
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
                .collect();
            cleaned.parse().ok()
        }
        _ => None,
    }
}

/// Read a JSON value as a list of trimmed strings: an array, or a comma-separated
/// string (which is what a CSV cell holding several tags looks like).
pub(super) fn as_list(raw: &Value) -> Vec<String> {
    match raw {
        Value::Array(items) => items.iter().filter_map(as_text).collect(),
        Value::String(s) => s
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect(),
        Value::Null => Vec::new(),
        other => as_text(other).into_iter().collect(),
    }
}

/// Whether a normalized value counts as "set". Used by required, by unique, by
/// `fill_blanks` import and by merge's default resolution.
pub fn is_empty_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

/// Normalize ONE value against ONE field's type. `Ok(None)` means "this clears the
/// field"; `Ok(Some(v))` is the canonical stored form.
///
/// Currency deserves its own note, because the rule is not guessable: **a JSON
/// INTEGER is already cents; a JSON string or float is a major-unit amount and gets
/// multiplied by 100.** That split is what lets the panel round-trip `12345` →
/// `12345` losslessly while a CSV cell reading `123.45` and an agent writing
/// `"$123.45"` both land on the same 12345.
pub fn validate_field_value(
    field: &Field,
    raw: &Value,
) -> std::result::Result<Option<Value>, FieldValidationError> {
    let invalid = |message: &str| {
        FieldValidationError::coded(&field.id, &field.slug, ValidationCode::Invalid, message)
    };
    let out_of_range = |message: &str| {
        FieldValidationError::coded(&field.id, &field.slug, ValidationCode::OutOfRange, message)
    };

    if raw.is_null() {
        return Ok(None);
    }

    match field.field_type {
        FieldType::Text | FieldType::LongText | FieldType::User => {
            Ok(as_text(raw).map(Value::String))
        }
        FieldType::Email => {
            let Some(text) = as_text(raw) else {
                return Ok(None);
            };
            let lowered = text.to_lowercase();
            let ok = !lowered.contains(char::is_whitespace)
                && lowered.matches('@').count() == 1
                && lowered.split_once('@').is_some_and(|(user, host)| {
                    !user.is_empty()
                        && host.contains('.')
                        && !host.starts_with('.')
                        && !host.ends_with('.')
                });
            if !ok {
                return Err(invalid("not a valid email address"));
            }
            Ok(Some(Value::String(lowered)))
        }
        FieldType::Phone => {
            let Some(text) = as_text(raw) else {
                return Ok(None);
            };
            // Deliberately permissive: phone formats are a swamp, and rejecting a
            // number a user can read is worse than storing an odd one. Only the
            // obviously-not-a-number case fails.
            if !text.chars().any(|c| c.is_ascii_digit()) {
                return Err(invalid("a phone number must contain at least one digit"));
            }
            Ok(Some(Value::String(text)))
        }
        FieldType::Url => {
            let Some(text) = as_text(raw) else {
                return Ok(None);
            };
            if text.contains(char::is_whitespace) {
                return Err(invalid("a URL must not contain spaces"));
            }
            let normalized = if text.starts_with("http://") || text.starts_with("https://") {
                text
            } else if text.contains('.') {
                // "acme.com" is what a human types and what every CSV holds.
                format!("https://{text}")
            } else {
                return Err(invalid("not a valid URL"));
            };
            Ok(Some(Value::String(normalized)))
        }
        FieldType::Number => {
            let Some(n) = as_number(raw) else {
                return Err(invalid("expected a number"));
            };
            Ok(Some(number_value(n)))
        }
        FieldType::Currency => {
            // See the fn docs: integer ⇒ already cents, anything else ⇒ major units.
            let cents = match raw {
                Value::Number(n) if n.is_i64() => n.as_i64().unwrap_or_default(),
                other => {
                    let Some(major) = as_number(other) else {
                        return Err(invalid("expected an amount"));
                    };
                    (major * 100.0).round() as i64
                }
            };
            Ok(Some(Value::Number(cents.into())))
        }
        FieldType::Percent => {
            let Some(n) = as_number(raw) else {
                return Err(invalid("expected a percentage"));
            };
            if !(0.0..=100.0).contains(&n) {
                return Err(out_of_range("a percentage must be between 0 and 100"));
            }
            Ok(Some(number_value(n)))
        }
        FieldType::Rating => {
            let Some(n) = as_number(raw) else {
                return Err(invalid("expected a rating"));
            };
            let max = i64::from(field.config.max_rating());
            let rounded = n.round() as i64;
            if rounded < 0 || rounded > max {
                return Err(out_of_range(&format!(
                    "a rating must be between 0 and {max}"
                )));
            }
            Ok(Some(Value::Number(rounded.into())))
        }
        FieldType::Checkbox => {
            let value = match raw {
                Value::Bool(b) => *b,
                Value::Number(n) => n.as_f64().unwrap_or_default() != 0.0,
                Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                    "true" | "yes" | "y" | "1" | "on" | "checked" => true,
                    "false" | "no" | "n" | "0" | "off" | "" => false,
                    _ => return Err(invalid("expected true or false")),
                },
                _ => return Err(invalid("expected true or false")),
            };
            Ok(Some(Value::Bool(value)))
        }
        FieldType::Date => {
            let Some(text) = as_text(raw) else {
                return Ok(None);
            };
            normalize_date(&text)
                .map(Value::String)
                .map(Some)
                .ok_or_else(|| invalid("not a valid date"))
        }
        FieldType::Datetime => {
            let Some(text) = as_text(raw) else {
                return Ok(None);
            };
            normalize_datetime(&text)
                .map(Value::String)
                .map(Some)
                .ok_or_else(|| invalid("not a valid date and time"))
        }
        FieldType::Select | FieldType::Status => {
            let Some(text) = as_text(raw) else {
                return Ok(None);
            };
            match field.config.resolve_option(&text) {
                Some(option) => Ok(Some(Value::String(option.id.clone()))),
                None => Err(FieldValidationError::coded(
                    &field.id,
                    &field.slug,
                    ValidationCode::UnknownOption,
                    format!("\"{text}\" is not one of this field's options"),
                )),
            }
        }
        FieldType::MultiSelect => {
            let raw_items = as_list(raw);
            if raw_items.is_empty() {
                return Ok(None);
            }
            let mut ids = Vec::with_capacity(raw_items.len());
            for item in raw_items {
                let Some(option) = field.config.resolve_option(&item) else {
                    return Err(FieldValidationError::coded(
                        &field.id,
                        &field.slug,
                        ValidationCode::UnknownOption,
                        format!("\"{item}\" is not one of this field's options"),
                    ));
                };
                if !ids.contains(&option.id) {
                    ids.push(option.id.clone());
                }
            }
            Ok(Some(json!(ids)))
        }
        FieldType::Relation => {
            let ids = as_list(raw);
            if ids.is_empty() {
                return Ok(None);
            }
            if field.config.relation_object_id.is_none() {
                return Err(FieldValidationError::coded(
                    &field.id,
                    &field.slug,
                    ValidationCode::BadRelationTarget,
                    "this relation field has no target object configured",
                ));
            }
            if !field.config.relation_multiple && ids.len() > 1 {
                return Err(out_of_range("this relation accepts a single record"));
            }
            let mut unique = Vec::with_capacity(ids.len());
            for id in ids {
                if !unique.contains(&id) {
                    unique.push(id);
                }
            }
            Ok(Some(json!(unique)))
        }
    }
}

/// Store an f64 as an integer when it is one, so a whole number round-trips as `5`
/// rather than `5.0` and `json_extract` comparisons stay integral.
pub(super) fn number_value(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::Number((n as i64).into())
    } else {
        serde_json::Number::from_f64(n)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

/// Validate a whole bag against an object's fields, adding the two checks that need
/// the database.
///
/// `partial` = the caller sent a MERGE update, so absent required fields are fine.
/// `exclude_record_id` is the record being updated, excluded from its own uniqueness
/// check — without it every save of an unchanged unique field would fail.
pub(super) fn validate_bag(
    conn: &Connection,
    object_id: &str,
    fields: &[Field],
    incoming: &ValueBag,
    partial: bool,
    exclude_record_id: Option<&str>,
) -> Result<ValidatedValues> {
    let index = field_index(fields);
    let mut out = ValidatedValues::default();

    for (key, raw) in incoming {
        let Some(field) = index.get(key.as_str()) else {
            out.errors.push(FieldValidationError::unknown_field(key));
            continue;
        };
        match validate_field_value(field, raw) {
            Ok(Some(value)) => {
                out.values.insert(field.slug.clone(), value);
            }
            // An explicit clear: recorded as JSON null so a MERGE update can tell
            // "clear this" from "do not mention this".
            Ok(None) => {
                out.values.insert(field.slug.clone(), Value::Null);
            }
            Err(error) => out.errors.push(error),
        }
    }

    // Relation targets: exist, are live, and are on the right object.
    for field in fields
        .iter()
        .filter(|f| f.field_type == FieldType::Relation)
    {
        let Some(Value::Array(targets)) = out.values.get(&field.slug) else {
            continue;
        };
        let Some(target_object) = field.config.relation_object_id.as_deref() else {
            continue;
        };
        for target in targets {
            let Some(id) = target.as_str() else { continue };
            let ok: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM records WHERE id = ?1 AND object_id = ?2 AND deleted_at IS NULL",
                    params![id, target_object],
                    |r| r.get(0),
                )
                .optional()?;
            if ok.is_none() {
                out.errors.push(FieldValidationError::coded(
                    &field.id,
                    &field.slug,
                    ValidationCode::BadRelationTarget,
                    format!("no live record \"{id}\" on the target object"),
                ));
            }
        }
    }

    // Uniqueness. Empty values never collide — a hundred people with no email are
    // not a hundred duplicates.
    for field in fields.iter().filter(|f| f.is_unique) {
        let Some(value) = out.values.get(&field.slug) else {
            continue;
        };
        if is_empty_value(value) {
            continue;
        }
        let Some(text) = as_text(value) else { continue };
        let sql = format!(
            "SELECT id FROM records
              WHERE object_id = ?1 AND deleted_at IS NULL AND id <> ?2
                AND lower(trim(CAST(json_extract(data, '$.{}') AS TEXT))) = lower(trim(?3))
              LIMIT 1",
            field.slug
        );
        let clash: Option<String> = conn
            .query_row(
                &sql,
                params![object_id, exclude_record_id.unwrap_or(""), text],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other) = clash {
            out.errors.push(FieldValidationError::coded(
                &field.id,
                &field.slug,
                ValidationCode::NotUnique,
                format!(
                    "another record ({other}) already has this {}",
                    field.name.to_lowercase()
                ),
            ));
        }
    }

    // Required, for a full write only.
    if !partial {
        for field in fields.iter().filter(|f| f.is_required) {
            let present = out
                .values
                .get(&field.slug)
                .is_some_and(|v| !is_empty_value(v));
            if !present {
                out.errors.push(FieldValidationError::coded(
                    &field.id,
                    &field.slug,
                    ValidationCode::Required,
                    format!("{} is required", field.name),
                ));
            }
        }
    }

    Ok(out)
}

/// Apply a field's `default_value` to every field the bag does not mention. Called
/// on create only — a default that reasserted itself on every update would make a
/// deliberately cleared field un-clearable.
pub(super) fn apply_defaults(fields: &[Field], values: &mut ValueBag) {
    for field in fields {
        let Some(default) = field.config.default_value.as_ref() else {
            continue;
        };
        let missing = values.get(&field.slug).map_or(true, |v| v.is_null());
        if missing {
            values.insert(field.slug.clone(), default.clone());
        }
    }
}

/// Drop the explicit-clear nulls a MERGE update produced, since a stored bag holds
/// only set values. Keeping nulls would make `json_extract` return SQL NULL either
/// way but bloat every row.
pub(super) fn prune_nulls(values: &mut ValueBag) {
    values.retain(|_, v| !v.is_null());
}

/// The record's display name: the object's `title_field_id` value, falling back to
/// the first text-ish field, then to a placeholder.
pub(super) fn compute_title(object: &Object, fields: &[Field], values: &ValueBag) -> String {
    let from_field =
        |field: &Field| -> Option<String> { values.get(&field.slug).and_then(as_text) };
    if let Some(title_field) = object
        .title_field_id
        .as_deref()
        .and_then(|id| fields.iter().find(|f| f.id == id))
    {
        if let Some(text) = from_field(title_field) {
            return text;
        }
    }
    for field in fields
        .iter()
        .filter(|f| matches!(f.field_type, FieldType::Text | FieldType::Email))
    {
        if let Some(text) = from_field(field) {
            return text;
        }
    }
    format!("Untitled {}", object.singular.to_lowercase())
}

/// The text FTS indexes for a record: every searchable field's value, space-joined.
/// See [`FieldType::is_searchable`] for why numbers, dates and option ids are out.
pub(super) fn fts_body(fields: &[Field], values: &ValueBag) -> String {
    let mut parts: Vec<String> = Vec::new();
    for field in fields.iter().filter(|f| f.field_type.is_searchable()) {
        if let Some(text) = values.get(&field.slug).and_then(as_text) {
            parts.push(text);
        }
    }
    parts.join(" ")
}

/// Replace a record's FTS row. Delete-then-insert keyed on `records.rowid`, which is
/// an O(log n) lookup — see the DDL comment on `records_fts`.
pub(super) fn fts_reindex(
    conn: &Connection,
    record_id: &str,
    title: &str,
    body: &str,
) -> Result<()> {
    let rowid: Option<i64> = conn
        .query_row(
            "SELECT rowid FROM records WHERE id = ?1",
            params![record_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(rowid) = rowid else { return Ok(()) };
    conn.execute("DELETE FROM records_fts WHERE rowid = ?1", params![rowid])?;
    conn.execute(
        "INSERT INTO records_fts(rowid, title, body) VALUES (?1, ?2, ?3)",
        params![rowid, title, body],
    )?;
    Ok(())
}

pub(super) fn fts_delete(conn: &Connection, record_id: &str) -> Result<()> {
    let rowid: Option<i64> = conn
        .query_row(
            "SELECT rowid FROM records WHERE id = ?1",
            params![record_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(rowid) = rowid {
        conn.execute("DELETE FROM records_fts WHERE rowid = ?1", params![rowid])?;
    }
    Ok(())
}

/// Turn arbitrary user input into a safe FTS5 MATCH expression.
///
/// FTS5's query language has operators (`AND`, `NEAR`, `*`, `"`, `:`), and passing
/// raw input straight through turns a search box into a syntax-error generator at
/// best. Every token is quoted, which makes it a literal; the last one gets a `*` so
/// typing narrows as you go.
pub(super) fn fts_match_expression(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '@' && c != '.' && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let last = tokens.len() - 1;
    Some(
        tokens
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if i == last {
                    format!("\"{t}\"*")
                } else {
                    format!("\"{t}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}
