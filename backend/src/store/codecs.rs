// ── Column lists + row decoders ────────────────────────────────────────────────
//
// Declared once so a decoder and its SELECTs cannot drift apart. Every decoder is
// TOLERANT: a value that fails to parse degrades to a documented default rather
// than failing the whole query, so one corrupt row cannot blank a table.

pub(super) const COLS_OBJECT: &str =
    "id, slug, singular, plural, icon, description, title_field_id, \
                           is_standard, position, created_at, updated_at";
pub(super) const COLS_FIELD: &str =
    "id, object_id, list_id, slug, name, field_type, config, description, \
                          is_required, is_unique, is_system, position, created_at, updated_at";
pub(super) const COLS_RECORD: &str =
    "id, object_id, title, data, deleted_at, created_by, created_at, updated_at";
pub(super) const COLS_LINK: &str =
    "id, field_id, source_record_id, source_object_id, target_record_id, \
                         target_object_id, created_at";
pub(super) const COLS_VIEW: &str = "id, object_id, name, kind, filter, sorts, visible_fields, \
                         group_by_field_id, is_default, position, created_at, updated_at";
pub(super) const COLS_LIST: &str =
    "id, object_id, name, description, icon, position, created_at, updated_at";
pub(super) const COLS_LIST_ENTRY: &str =
    "id, list_id, record_id, data, position, created_at, updated_at";
pub(super) const COLS_ACTIVITY: &str =
    "id, record_id, object_id, kind, title, body, field_id, from_value, \
                             to_value, assignee, due_at, completed_at, due_notified_at, author, \
                             metadata, created_at, updated_at";
pub(super) const COLS_IMPORT: &str = "id, object_id, filename, status, delimiter, has_header, row_count, \
                           columns, mappings, dedupe, preview, result, error, created_at, updated_at";

/// Decode a JSON TEXT column, falling back to a default on anything unparseable.
pub(super) fn decode_json<T: serde::de::DeserializeOwned + Default>(raw: Option<String>) -> T {
    raw.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub(super) fn encode_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

pub(super) fn row_to_object(row: &Row<'_>) -> rusqlite::Result<Object> {
    Ok(Object {
        id: row.get(0)?,
        slug: row.get(1)?,
        singular: row.get(2)?,
        plural: row.get(3)?,
        icon: row.get(4)?,
        description: row.get(5)?,
        title_field_id: row.get(6)?,
        is_standard: row.get::<_, i64>(7)? != 0,
        position: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

pub(super) fn row_to_field(row: &Row<'_>) -> rusqlite::Result<Field> {
    let field_type: String = row.get(5)?;
    let config: String = row.get(6)?;
    Ok(Field {
        id: row.get(0)?,
        object_id: row.get(1)?,
        list_id: row.get(2)?,
        slug: row.get(3)?,
        name: row.get(4)?,
        field_type: FieldType::from_db(&field_type),
        config: FieldConfig::decode(&config),
        description: row.get(7)?,
        is_required: row.get::<_, i64>(8)? != 0,
        is_unique: row.get::<_, i64>(9)? != 0,
        is_system: row.get::<_, i64>(10)? != 0,
        position: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

pub(super) fn row_to_record(row: &Row<'_>) -> rusqlite::Result<Record> {
    let data: String = row.get(3)?;
    Ok(Record {
        id: row.get(0)?,
        object_id: row.get(1)?,
        title: row.get(2)?,
        values: serde_json::from_str(&data).unwrap_or_default(),
        deleted_at: row.get(4)?,
        created_by: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

pub(super) fn row_to_link(row: &Row<'_>) -> rusqlite::Result<RecordLink> {
    Ok(RecordLink {
        id: row.get(0)?,
        field_id: row.get(1)?,
        source_record_id: row.get(2)?,
        source_object_id: row.get(3)?,
        target_record_id: row.get(4)?,
        target_object_id: row.get(5)?,
        created_at: row.get(6)?,
    })
}

pub(super) fn row_to_view(row: &Row<'_>) -> rusqlite::Result<View> {
    let kind: String = row.get(3)?;
    let filter: Option<String> = row.get(4)?;
    Ok(View {
        id: row.get(0)?,
        object_id: row.get(1)?,
        name: row.get(2)?,
        kind: ViewKind::from_db(&kind),
        filter: filter.and_then(|f| serde_json::from_str(&f).ok()),
        sorts: decode_json(row.get(5)?),
        visible_field_ids: decode_json(row.get(6)?),
        group_by_field_id: row.get(7)?,
        is_default: row.get::<_, i64>(8)? != 0,
        position: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

pub(super) fn row_to_list(row: &Row<'_>) -> rusqlite::Result<List> {
    Ok(List {
        id: row.get(0)?,
        object_id: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        icon: row.get(4)?,
        position: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

pub(super) fn row_to_list_entry(row: &Row<'_>) -> rusqlite::Result<ListEntry> {
    let data: String = row.get(3)?;
    Ok(ListEntry {
        id: row.get(0)?,
        list_id: row.get(1)?,
        record_id: row.get(2)?,
        values: serde_json::from_str(&data).unwrap_or_default(),
        position: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

pub(super) fn row_to_activity(row: &Row<'_>) -> rusqlite::Result<Activity> {
    let kind: String = row.get(3)?;
    let from_value: Option<String> = row.get(7)?;
    let to_value: Option<String> = row.get(8)?;
    let metadata: Option<String> = row.get(14)?;
    Ok(Activity {
        id: row.get(0)?,
        record_id: row.get(1)?,
        object_id: row.get(2)?,
        kind: ActivityKind::from_db(&kind),
        title: row.get(4)?,
        body: row.get(5)?,
        field_id: row.get(6)?,
        from_value: from_value.and_then(|v| serde_json::from_str(&v).ok()),
        to_value: to_value.and_then(|v| serde_json::from_str(&v).ok()),
        assignee: row.get(9)?,
        due_at: row.get(10)?,
        completed_at: row.get(11)?,
        due_notified_at: row.get(12)?,
        author: row.get(13)?,
        metadata: metadata.and_then(|v| serde_json::from_str(&v).ok()),
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

pub(super) fn row_to_import(row: &Row<'_>) -> rusqlite::Result<ImportJob> {
    let status: String = row.get(3)?;
    let preview: Option<String> = row.get(10)?;
    let result: Option<String> = row.get(11)?;
    Ok(ImportJob {
        id: row.get(0)?,
        object_id: row.get(1)?,
        filename: row.get(2)?,
        status: ImportStatus::from_db(&status),
        delimiter: row.get(4)?,
        has_header: row.get::<_, i64>(5)? != 0,
        row_count: row.get::<_, i64>(6)?.max(0) as usize,
        columns: decode_json(row.get(7)?),
        mappings: decode_json(row.get(8)?),
        dedupe: decode_json(row.get(9)?),
        preview: preview.and_then(|p| serde_json::from_str(&p).ok()),
        result: result.and_then(|r| serde_json::from_str(&r).ok()),
        error: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

// ── Internal loaders (all take `&Connection`, never `&self`) ───────────────────
//
// See the module docs on locking: a public method that called another public method
// would deadlock on the non-reentrant async mutex. Everything shared lives here.

/// Resolve an object by id OR slug. The tolerance is what lets an agent tool say
/// `"deal"` where the panel says `"obj_deal"`.
pub(super) fn load_object(conn: &Connection, id_or_slug: &str) -> Result<Option<Object>> {
    let sql = format!("SELECT {COLS_OBJECT} FROM objects WHERE id = ?1 OR slug = ?1");
    Ok(conn
        .query_row(&sql, params![id_or_slug], row_to_object)
        .optional()?)
}

/// An object's own fields (`list_id IS NULL`), in position order.
pub(super) fn load_fields(conn: &Connection, object_id: &str) -> Result<Vec<Field>> {
    let sql = format!(
        "SELECT {COLS_FIELD} FROM fields WHERE object_id = ?1 AND list_id IS NULL
         ORDER BY position ASC, created_at ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![object_id], row_to_field)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One list's extra fields, in position order.
pub(super) fn load_list_fields(conn: &Connection, list_id: &str) -> Result<Vec<Field>> {
    let sql = format!(
        "SELECT {COLS_FIELD} FROM fields WHERE list_id = ?1
         ORDER BY position ASC, created_at ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![list_id], row_to_field)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(super) fn load_field(conn: &Connection, field_id: &str) -> Result<Option<Field>> {
    let sql = format!("SELECT {COLS_FIELD} FROM fields WHERE id = ?1");
    Ok(conn
        .query_row(&sql, params![field_id], row_to_field)
        .optional()?)
}

pub(super) fn load_record(conn: &Connection, record_id: &str) -> Result<Option<Record>> {
    let sql = format!("SELECT {COLS_RECORD} FROM records WHERE id = ?1");
    Ok(conn
        .query_row(&sql, params![record_id], row_to_record)
        .optional()?)
}

pub(super) fn load_list(conn: &Connection, list_id: &str) -> Result<Option<List>> {
    let sql = format!("SELECT {COLS_LIST} FROM lists WHERE id = ?1");
    Ok(conn
        .query_row(&sql, params![list_id], row_to_list)
        .optional()?)
}

/// An id-AND-slug lookup table over a field set, so a filter/sort/mapping may name
/// either. Both keys point at the same `Field`.
pub(super) fn field_index(fields: &[Field]) -> HashMap<String, Field> {
    let mut index = HashMap::with_capacity(fields.len() * 2);
    for field in fields {
        index.insert(field.id.clone(), field.clone());
        index.insert(field.slug.clone(), field.clone());
    }
    index
}

/// The next free `position` for a new field/view/list/entry in a scope.
pub(super) fn next_position(conn: &Connection, sql: &str, key: &str) -> Result<i64> {
    let max: Option<i64> = conn
        .query_row(sql, params![key], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(max.unwrap_or(-1) + 1)
}
