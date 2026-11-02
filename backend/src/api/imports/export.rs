//! CSV export of a view, and column type inference for uploads.

use super::*;

// ── Export ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub(super) struct ExportQuery {
    /// Full-text pre-filter, ANDed with the view's own filter exactly as the on-screen
    /// search box is — so "export what I am looking at" exports what they are looking
    /// at.
    #[serde(default)]
    pub(super) search: Option<String>,
    #[serde(default)]
    pub(super) include_deleted: bool,
    /// Row ceiling, clamped to [`EXPORT_MAX_ROWS`].
    #[serde(default)]
    pub(super) limit: Option<usize>,
    /// Output delimiter. One character; defaults to `,`.
    #[serde(default)]
    pub(super) delimiter: Option<String>,
}

/// The response type: headers plus a body. A tuple rather than `Response::builder()`
/// so there is no infallible-in-practice `http::Error` to map into a 500.
pub(super) type CsvResponse = ([(HeaderName, String); 5], String);

/// `GET /exports/views/:view_id` — the view's current result set as CSV.
pub(super) async fn export_view(
    State(state): State<AppState>,
    Path(view_id): Path<String>,
    Query(query): Query<ExportQuery>,
) -> ApiResult<CsvResponse> {
    let delimiter = match query.delimiter.as_deref() {
        Some(raw) => {
            let mut chars = raw.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => c,
                _ => {
                    return Err(ApiError::bad_request(
                        "a delimiter must be exactly one character",
                    ))
                }
            }
        }
        None => ',',
    };
    let ceiling = query
        .limit
        .filter(|n| *n > 0)
        .unwrap_or(EXPORT_MAX_ROWS)
        .min(EXPORT_MAX_ROWS);
    let overrides = ViewQueryOverrides {
        search: query
            .search
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        include_deleted: query.include_deleted,
        ..Default::default()
    };

    // Paged rather than one big query: `run_view` uses the limit it is given verbatim,
    // and the store's page ceiling exists for a reason. Each call takes the store lock
    // once and releases it before the next — no future is held across another.
    let page_size = state.config.max_page_size.max(1);
    let mut records: Vec<Record> = Vec::new();
    let mut fields: Vec<Field> = Vec::new();
    let mut view_name = String::new();
    let mut total: i64 = 0;
    let mut truncated = false;
    let mut offset = 0usize;
    loop {
        let want = page_size.min(ceiling - records.len());
        let Some(result) = state
            .store
            .run_view(&view_id, &overrides, want, offset)
            .await?
        else {
            return Err(ApiError::not_found("view"));
        };
        if records.is_empty() {
            fields = result.fields;
            view_name = result.view.name;
            total = result.page.total;
        }
        let got = result.page.items.len();
        let has_more = result.page.has_more;
        records.extend(result.page.items);
        offset += got;
        if got == 0 || !has_more {
            break;
        }
        if records.len() >= ceiling {
            truncated = true;
            break;
        }
    }

    let body = render_csv(&fields, &records, delimiter);
    // `slugify` closes the alphabet to `[a-z0-9_]`, which is also what makes it safe
    // to interpolate into a header value — a view named `"; rm -rf /` cannot become
    // one.
    let filename = format!(
        "{}.csv",
        slugify(&view_name).unwrap_or_else(|| "export".to_string())
    );
    Ok((
        [
            (CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
            (
                HeaderName::from_static(HDR_EXPORT_ROWS),
                records.len().to_string(),
            ),
            (HeaderName::from_static(HDR_EXPORT_TOTAL), total.to_string()),
            (
                HeaderName::from_static(HDR_EXPORT_TRUNCATED),
                truncated.to_string(),
            ),
        ],
        body,
    ))
}

/// Header row plus one row per record.
///
/// `id` leads and the two timestamps trail, so the human-meaningful columns sit where
/// a spreadsheet opens. All three are unmapped on a re-import (they are reserved
/// slugs, which is why no field can ever be called `id`), so the file round-trips
/// without them fighting a real column.
pub(super) fn render_csv(fields: &[Field], records: &[Record], delimiter: char) -> String {
    let mut out = String::new();
    let mut header: Vec<String> = Vec::with_capacity(fields.len() + 3);
    header.push("id".to_string());
    header.extend(fields.iter().map(|f| f.name.clone()));
    header.push("created_at".to_string());
    header.push("updated_at".to_string());
    write_row(&mut out, &header, delimiter);

    for record in records {
        let mut row: Vec<String> = Vec::with_capacity(fields.len() + 3);
        row.push(record.id.clone());
        for field in fields {
            row.push(export_cell(field, record.values.get(&field.slug)));
        }
        row.push(record.created_at.clone());
        row.push(record.updated_at.clone());
        write_row(&mut out, &row, delimiter);
    }
    out
}

pub(super) fn write_row(out: &mut String, cells: &[String], delimiter: char) {
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push(delimiter);
        }
        out.push_str(&csv_cell(cell, delimiter));
    }
    out.push('\n');
}

/// Escape one cell for [`crate::store::parse_csv`] to read back identically.
///
/// Quoted when it holds the delimiter, a quote, a newline, or edge whitespace the
/// reader would trim on the way back in. Inside quotes, `"` doubles — the exact rule
/// that reader implements.
pub(super) fn csv_cell(raw: &str, delimiter: char) -> String {
    let needs_quotes = raw.contains(delimiter)
        || raw.contains('"')
        || raw.contains('\n')
        || raw.contains('\r')
        || raw.trim() != raw;
    if !needs_quotes {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for ch in raw.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Render one stored value the way a human reads it AND the way this app's own
/// importer parses it back. Where those two pull apart, the round trip wins:
///
/// * **currency stays integer cents** — the validator's rule is "a JSON integer is
///   already cents", so `123456` re-imports as $1,234.56. Writing `1234.56` would
///   re-import correctly too (a float is major units), but a spreadsheet that reads
///   the column as a number and writes back `1234.6` would then silently lose a cent
///   per row, whereas cents are exact.
/// * **options carry LABELS** — `resolve_option` accepts an id or a label, and a CSV
///   full of `opt_deal_stage_proposal` is unreadable.
/// * **multi-valued cells are comma-joined** — `as_list` splits a string on commas, so
///   `Enterprise, Priority` comes back as the same two options.
/// * **relations carry record IDS** — titles are not unique and would re-import as a
///   bad relation target, which is a worse trade than an opaque column.
pub(super) fn export_cell(field: &Field, value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    match value {
        Value::Null => String::new(),
        Value::String(s) => {
            if field.field_type.is_option_backed() {
                field
                    .config
                    .option(s)
                    .map(|o| o.label.clone())
                    .unwrap_or_else(|| s.clone())
            } else {
                s.clone()
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(s) if field.field_type.is_option_backed() => field
                    .config
                    .option(s)
                    .map(|o| o.label.clone())
                    .unwrap_or_else(|| s.clone()),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        // No field type stores an object today. Emitting compact JSON rather than
        // an empty cell means a schema that grows one is visible in the export
        // instead of silently blank.
        other => other.to_string(),
    }
}

// ── Column type inference ──────────────────────────────────────────────────────

/// Guess the field type a column wants, from its own sample values.
///
/// Advisory only — it decides what the "create a field" form opens on, never what an
/// existing field is (a field's type is immutable, by design). Every rule requires
/// EVERY sample to agree: one address in a column of names must not turn the column
/// into an email field, because the import would then reject every other row.
///
/// Option-backed types are never inferred. A `select` with no options rejects every
/// value, so guessing one would produce a field that cannot accept the column that
/// suggested it.
pub(super) fn infer_field_type(samples: &[String]) -> FieldType {
    let samples: Vec<&str> = samples
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if samples.is_empty() {
        return FieldType::Text;
    }
    let all = |f: fn(&str) -> bool| samples.iter().all(|s| f(s));

    if all(is_boolean_word) {
        return FieldType::Checkbox;
    }
    if all(looks_like_email) {
        return FieldType::Email;
    }
    if all(looks_like_url) {
        return FieldType::Url;
    }
    if all(looks_like_money) {
        return FieldType::Currency;
    }
    if all(|s| s.parse::<f64>().is_ok()) {
        return FieldType::Number;
    }
    // `normalize_date` also accepts a full datetime (it truncates), so a bare-date
    // shape is what separates the two. Ask for the strict shape first.
    if all(|s| s.len() == 10 && normalize_date(s).as_deref() == Some(s)) {
        return FieldType::Date;
    }
    if all(|s| normalize_datetime(s).is_some()) {
        return FieldType::Datetime;
    }
    if samples
        .iter()
        .any(|s| s.chars().count() > LONG_TEXT_SAMPLE_LEN)
    {
        return FieldType::LongText;
    }
    FieldType::Text
}

pub(super) fn is_boolean_word(raw: &str) -> bool {
    matches!(
        raw.to_ascii_lowercase().as_str(),
        "true" | "false" | "yes" | "no" | "y" | "n"
    )
}

pub(super) fn looks_like_email(raw: &str) -> bool {
    !raw.contains(char::is_whitespace)
        && raw.matches('@').count() == 1
        && raw
            .split_once('@')
            .is_some_and(|(user, host)| !user.is_empty() && host.contains('.'))
}

pub(super) fn looks_like_url(raw: &str) -> bool {
    (raw.starts_with("http://") || raw.starts_with("https://"))
        && !raw.contains(char::is_whitespace)
}

/// A leading currency symbol is the only signal that separates "amount" from "number",
/// and it is the one every exporting CRM emits.
pub(super) fn looks_like_money(raw: &str) -> bool {
    let Some(rest) = raw.strip_prefix(['$', '€', '£', '¥']) else {
        return false;
    };
    let digits: String = rest.chars().filter(|c| *c != ',' && *c != ' ').collect();
    !digits.is_empty() && digits.parse::<f64>().is_ok()
}
