//! Domain types for Harbor — the wire contract the sidecar serves, the store
//! persists, and the desktop dock panel renders from.
//!
//! Conventions, applied uniformly and deliberately:
//!
//! - **Ids are ULID-ish TEXT with a typed prefix** (`obj_`, `fld_`, `rec_`, `lnk_`,
//!   `view_`, `lst_`, `ent_`, `act_`, `imp_`, `opt_`). Two properties are
//!   load-bearing. The prefix is not decoration: this app is one big graph of
//!   cross-table references with no FK constraint behind them (see [`crate::store`]
//!   for why), so a mis-wired id is otherwise invisible until it silently matches
//!   nothing. And the body is **lexicographically time-ordered** (Crockford-base32
//!   millis then randomness, exactly ULID's layout), which is what lets every
//!   paginated list use `ORDER BY … , id` as a stable, non-repeating tiebreaker.
//! - **Timestamps are RFC-3339 STRINGS**, always UTC, always millisecond precision,
//!   always `Z`-suffixed — `2026-08-10T14:03:11.482Z`. Fixed precision plus a fixed
//!   suffix is what makes lexicographic `<=` on the TEXT column agree with real
//!   time, so `due_at <= ?1` is a correct index range scan and not a string-sort
//!   accident. Produce them ONLY through [`now_rfc3339`] / [`normalize_datetime`];
//!   never hand-format one.
//! - **Money is integer CENTS** (`i64`), never a float. The currency code lives in
//!   the field's [`FieldConfig`], not next to each amount, because a currency field
//!   is single-currency by definition — a per-value currency is a different feature
//!   and pretending otherwise produces sums that are silently meaningless.
//! - **Booleans are `bool` on the wire, `INTEGER` 0/1 in SQLite.**
//! - **Every field name is snake_case on the wire**, including inside the record
//!   value bags.
//!
//! ## The one structural decision everything else follows from
//!
//! A record is `object_id` plus a **JSON value bag keyed by FIELD SLUG**, not a row
//! in a per-object table. That is what makes the schema user-editable at runtime
//! without DDL. Two consequences a later agent must not fight:
//!
//! 1. **A field's `slug` is immutable after creation.** [`UpdateFieldRequest`] has no
//!    `slug`, and that is not an oversight: renaming a slug would have to rewrite
//!    every record's bag, every view filter, every import mapping and every stored
//!    board `group_by` in one transaction, and any of those it missed would silently
//!    read as "empty". Renaming the *display* `name` is free and is what the UI
//!    offers.
//! 2. **Everything on the wire references a field by `field_id`**, and the store
//!    resolves id → slug when it builds SQL. The tolerant resolvers
//!    (`store::resolve_field`) also accept a bare slug, because an agent-facing tool
//!    call says `"stage"`, not `"fld_deal_stage"` — the `fld_` prefix makes the two
//!    unambiguous.
//!
//! Enum columns carry no SQL `CHECK` constraint. The Rust enum plus its tolerant
//! `from_db` IS the guard: an unrecognised value degrades to a documented default
//! rather than failing a whole list query, which is what keeps one corrupt row from
//! blanking the panel.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A record's (or list entry's) values: field **slug** → JSON value.
///
/// A `serde_json::Map` rather than a `BTreeMap<String, Value>` so it round-trips
/// through `serde_json::from_str::<ValueBag>` with no intermediate allocation and
/// so `values["stage"]` indexing works the way every caller expects.
pub type ValueBag = serde_json::Map<String, Value>;

// ── Time ───────────────────────────────────────────────────────────────────────

/// Now, as an RFC-3339 UTC string with millisecond precision and a `Z` suffix.
///
/// Every `created_at` / `updated_at` / `deleted_at` in this app is produced here so
/// there is exactly one clock read to stub in tests, and exactly one place the
/// precision is decided. The precision is load-bearing: mixing
/// `…T14:03:11Z` and `…T14:03:11.482Z` in one column breaks lexicographic ordering
/// for the second within any given second.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Normalize any RFC-3339-ish instant a caller supplied into this app's canonical
/// form (UTC, millis, `Z`). Returns `None` when it does not parse.
///
/// Accepts offsets (`2026-08-10T16:03:11+02:00` → `2026-08-10T14:03:11.000Z`), so a
/// client in a non-UTC zone can send its own local instant and still land in a
/// comparable column.
pub fn normalize_datetime(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(
            dt.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );
    }
    // A bare `YYYY-MM-DD` is the shape a `date` field and every CSV export produce;
    // anchor it to midnight UTC rather than rejecting it.
    if let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let dt = date.and_hms_opt(0, 0, 0)?.and_utc();
        return Some(dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    }
    // `YYYY-MM-DDTHH:MM(:SS)` with no zone: read as UTC. A naive local reading would
    // make the same CSV import to two different values on two machines.
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, fmt) {
            return Some(
                dt.and_utc()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            );
        }
    }
    None
}

/// Normalize a `date`-typed value to a bare `YYYY-MM-DD`.
///
/// A `date` is stored WITHOUT a time because "close date = 2026-03-31" is a calendar
/// fact, not an instant: storing it as midnight-UTC would render as March 30th for
/// anyone west of Greenwich, which is the classic off-by-one-day CRM bug.
pub fn normalize_date(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        return Some(date.format("%Y-%m-%d").to_string());
    }
    normalize_datetime(trimmed).map(|dt| dt[..10].to_string())
}

// ── Ids ────────────────────────────────────────────────────────────────────────

/// Crockford base32 — ULID's alphabet. Excludes `I`, `L`, `O` and `U` so an id read
/// aloud or retyped from a support ticket cannot be ambiguous.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A fresh ULID-ish body: 10 chars of millisecond timestamp then 16 chars of
/// randomness, 26 chars total, sorting lexicographically in creation order.
///
/// Hand-rolled rather than pulling the `ulid` crate: the workspace does not already
/// carry it, and a new dependency churns the shared `Cargo.lock` for every other job
/// building this tree. The randomness comes from a v4 UUID, which the workspace does
/// carry.
pub fn new_ulid() -> String {
    let ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let mut out = String::with_capacity(26);
    for i in (0..10u32).rev() {
        out.push(CROCKFORD[((ms >> (i * 5)) & 0x1f) as usize] as char);
    }
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let mut acc: u128 = 0;
    for b in &bytes[..10] {
        acc = (acc << 8) | u128::from(*b);
    }
    for i in (0..16u32).rev() {
        out.push(CROCKFORD[((acc >> (i * 5)) & 0x1f) as usize] as char);
    }
    out
}

/// A fresh prefixed id. See the module docs for why the prefix exists.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", new_ulid())
}

pub const ID_OBJECT: &str = "obj_";
pub const ID_FIELD: &str = "fld_";
pub const ID_RECORD: &str = "rec_";
pub const ID_LINK: &str = "lnk_";
pub const ID_VIEW: &str = "view_";
pub const ID_LIST: &str = "lst_";
pub const ID_LIST_ENTRY: &str = "ent_";
pub const ID_ACTIVITY: &str = "act_";
pub const ID_IMPORT: &str = "imp_";
pub const ID_OPTION: &str = "opt_";

// ── Slugs ──────────────────────────────────────────────────────────────────────

/// Slugs that a user field may never take.
///
/// Every one of these is a real column on `records` or a special sort/filter key the
/// query builder resolves BEFORE it looks in the value bag (see
/// [`ViewSort::is_intrinsic`]). A field slugged `created_at` would make
/// `ORDER BY created_at` ambiguous — the row column or the bag entry? — and the
/// ambiguity would be resolved differently by sorting and by filtering.
pub const RESERVED_SLUGS: [&str; 8] = [
    "id",
    "object_id",
    "title",
    "created_at",
    "updated_at",
    "deleted_at",
    "record_id",
    "rowid",
];

/// Maximum slug length. Bounded because the slug is spliced into a
/// `json_extract(values, '$."<slug>"')` path.
pub const MAX_SLUG_LEN: usize = 64;

/// Whether `slug` is acceptable as a field slug: `^[a-z][a-z0-9_]{0,63}$` and not
/// reserved.
///
/// The character class is not cosmetic. A slug reaches SQL inside a JSON path
/// string, and while the path is passed as a bound parameter wherever possible, the
/// board/group and aggregate builders interpolate it. Restricting the alphabet to
/// `[a-z0-9_]` means there is no quote, no `$`, no `.` and no `[` to escape — the
/// injection surface is closed by construction rather than by remembering to quote.
pub fn is_valid_slug(slug: &str) -> bool {
    if slug.is_empty() || slug.len() > MAX_SLUG_LEN {
        return false;
    }
    if RESERVED_SLUGS.contains(&slug) {
        return false;
    }
    let mut chars = slug.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Best-effort slugification of a human label, for CSV column → field suggestions
/// and for the "create a field from this column" path. Returns `None` when nothing
/// valid survives.
pub fn slugify(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut last_underscore = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_underscore = false;
        } else if !last_underscore && !out.is_empty() {
            out.push('_');
            last_underscore = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        return None;
    }
    if !out.starts_with(|c: char| c.is_ascii_lowercase()) {
        out.insert_str(0, "f_");
    }
    out.truncate(MAX_SLUG_LEN);
    while out.ends_with('_') {
        out.pop();
    }
    if RESERVED_SLUGS.contains(&out.as_str()) {
        out.push_str("_field");
    }
    is_valid_slug(&out).then_some(out)
}

// ── Objects ────────────────────────────────────────────────────────────────────

/// One user-definable entity type. Attio's "object", Twenty's "custom object".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Object {
    pub id: String,
    /// Stable machine name, unique across the install. Immutable after creation for
    /// the same reason a field slug is (see the module docs).
    pub slug: String,
    /// "Company".
    pub singular: String,
    /// "Companies". Stored rather than derived because English pluralisation is not
    /// a function, and the user names their own objects.
    pub plural: String,
    /// Lucide-style icon name the panel renders. Free-form: the panel falls back to
    /// a generic icon for anything it does not know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The field whose value becomes [`Record::title`]. `None` only for a
    /// half-configured custom object; the store falls back to the first text field,
    /// then to the record id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_field_id: Option<String>,
    /// `true` for the five seeded objects. Standard objects cannot be deleted and
    /// their system fields cannot be removed — a CRM whose `deal` object can be
    /// dropped by a stray click is not a CRM.
    pub is_standard: bool,
    /// Sidebar order.
    pub position: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateObjectRequest {
    pub slug: String,
    pub singular: String,
    /// Defaults to `singular` + "s" when absent.
    #[serde(default)]
    pub plural: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateObjectRequest {
    #[serde(default)]
    pub singular: Option<String>,
    #[serde(default)]
    pub plural: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub title_field_id: Option<String>,
    #[serde(default)]
    pub position: Option<i64>,
}

/// An object plus the counts the sidebar shows, so rendering the nav is one request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectSummary {
    #[serde(flatten)]
    pub object: Object,
    pub field_count: i64,
    /// Live (non-soft-deleted) records.
    pub record_count: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_view_id: Option<String>,
}

/// One object with everything needed to render it, for the panel's boot call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectWithFields {
    #[serde(flatten)]
    pub object: Object,
    pub fields: Vec<Field>,
    pub views: Vec<View>,
    pub record_count: i64,
}

/// `GET /api/crm/schema` — the whole schema in one round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaResponse {
    pub objects: Vec<ObjectWithFields>,
    pub lists: Vec<List>,
}

// ── Fields ─────────────────────────────────────────────────────────────────────

/// The seventeen attribute types a field can have.
///
/// Declaration order is load-bearing: it is the order the field-type picker renders
/// in, so reordering these variants reorders the product.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Text,
    LongText,
    Number,
    /// Integer CENTS, with the currency in [`FieldConfig::currency_code`].
    Currency,
    /// Stored as a number in 0..=100, not 0..=1. The UI appends the `%`.
    Percent,
    Checkbox,
    /// Bare `YYYY-MM-DD`. See [`normalize_date`] for why it carries no time.
    Date,
    /// Full RFC-3339 instant.
    Datetime,
    /// Exactly one option id from [`FieldConfig::options`].
    Select,
    /// A `Vec` of option ids.
    MultiSelect,
    /// Like [`FieldType::Select`], but the options carry pipeline semantics
    /// (`is_won` / `is_lost`) and a change raises `deal.stage_changed`. An object
    /// may have more than one, but the FIRST by position is the one the pipeline
    /// report defaults to.
    Status,
    Email,
    Phone,
    Url,
    /// Integer 0..=`max_rating` (default 5).
    Rating,
    /// One or more record ids on the object named by
    /// [`FieldConfig::relation_object_id`]. Always stored as an ARRAY on the wire,
    /// even when [`FieldConfig::relation_multiple`] is false, so a client never has
    /// to branch on cardinality to read it.
    Relation,
    /// A Ryu user/assignee handle. An opaque string here: Harbor does not own the
    /// identity directory and must not pretend to.
    User,
}

impl FieldType {
    pub const ALL: [FieldType; 17] = [
        FieldType::Text,
        FieldType::LongText,
        FieldType::Number,
        FieldType::Currency,
        FieldType::Percent,
        FieldType::Checkbox,
        FieldType::Date,
        FieldType::Datetime,
        FieldType::Select,
        FieldType::MultiSelect,
        FieldType::Status,
        FieldType::Email,
        FieldType::Phone,
        FieldType::Url,
        FieldType::Rating,
        FieldType::Relation,
        FieldType::User,
    ];

    /// The wire/SQL value. Must stay byte-identical to the serde `rename_all`
    /// output above — it is the same string in both places.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::LongText => "long_text",
            Self::Number => "number",
            Self::Currency => "currency",
            Self::Percent => "percent",
            Self::Checkbox => "checkbox",
            Self::Date => "date",
            Self::Datetime => "datetime",
            Self::Select => "select",
            Self::MultiSelect => "multi_select",
            Self::Status => "status",
            Self::Email => "email",
            Self::Phone => "phone",
            Self::Url => "url",
            Self::Rating => "rating",
            Self::Relation => "relation",
            Self::User => "user",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }

    /// Tolerant decode for a row read back out of SQLite. An unknown type degrades
    /// to [`FieldType::Text`], which renders the raw value rather than blanking the
    /// whole object.
    pub fn from_db(s: &str) -> Self {
        Self::parse(s).unwrap_or(Self::Text)
    }

    /// Whether values of this type come from [`FieldConfig::options`].
    pub const fn is_option_backed(self) -> bool {
        matches!(self, Self::Select | Self::MultiSelect | Self::Status)
    }

    /// Whether a value of this type is a LIST on the wire. Multi-valued types are
    /// stored as JSON arrays and compared with `IS ANY OF` semantics.
    pub const fn is_multi(self) -> bool {
        matches!(self, Self::MultiSelect | Self::Relation)
    }

    /// Whether this type's stored value is numeric, which decides whether a filter
    /// comparison is numeric or lexicographic.
    pub const fn is_numeric(self) -> bool {
        matches!(
            self,
            Self::Number | Self::Currency | Self::Percent | Self::Rating
        )
    }

    /// Whether this type contributes its text to the FTS5 index. Numbers, dates and
    /// option ids do not: matching "5000" against every currency column produces
    /// noise, and an option id is not language.
    pub const fn is_searchable(self) -> bool {
        matches!(
            self,
            Self::Text | Self::LongText | Self::Email | Self::Phone | Self::Url | Self::User
        )
    }
}

impl std::str::FromStr for FieldType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("unknown field type \"{s}\""))
    }
}

/// One choice on a `select` / `multi_select` / `status` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOption {
    /// `opt_…`. STABLE across a relabel — this is what records store, so renaming
    /// "Proposal" to "Proposal sent" must not rewrite a single record.
    pub id: String,
    pub label: String,
    /// Design-token colour name the panel maps to a chip style (`blue`, `green`,
    /// `amber`, …). Free-form; unknown values fall back to the neutral chip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub position: i64,
    /// Terminal-success bucket, `status` fields only. The pipeline report's
    /// conversion rate is `won / entered`, so this flag is what makes the number
    /// mean anything.
    #[serde(default)]
    pub is_won: bool,
    /// Terminal-failure bucket, `status` fields only.
    #[serde(default)]
    pub is_lost: bool,
}

impl SelectOption {
    /// A non-terminal stage. `position` is the caller's; ids are generated unless
    /// the caller supplied one (seeds do).
    pub fn new(id: impl Into<String>, label: impl Into<String>, position: i64) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            color: None,
            position,
            is_won: false,
            is_lost: false,
        }
    }

    pub fn with_color(mut self, color: &str) -> Self {
        self.color = Some(color.to_string());
        self
    }

    pub fn won(mut self) -> Self {
        self.is_won = true;
        self
    }

    pub fn lost(mut self) -> Self {
        self.is_lost = true;
        self
    }

    /// Whether this option ends the pipeline, either way.
    pub const fn is_terminal(&self) -> bool {
        self.is_won || self.is_lost
    }
}

/// Everything type-specific about a field, in one JSON column.
///
/// A single struct with type-specific keys rather than an enum-per-type: the panel's
/// field editor reads and writes ALL of these on one form, and an enum would make
/// "change a select into a status" a destructive re-serialisation instead of a flag
/// flip. Unused keys are simply absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldConfig {
    /// `select` / `multi_select` / `status`. Ordered by [`SelectOption::position`]
    /// on the way in and out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<SelectOption>,
    /// `relation`: the object whose records this field points at. A relation field
    /// with no target is inert — validation rejects every value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation_object_id: Option<String>,
    /// `relation`: whether more than one target is allowed. The stored value is an
    /// array either way; this bounds its length to 1.
    #[serde(default)]
    pub relation_multiple: bool,
    /// `relation`: what the OTHER side calls this edge — "People" on a company's
    /// page for `person.company`.
    ///
    /// Harbor materialises ONE row per edge and queries it from both ends (see
    /// [`RecordLinkView`]), so there is no inverse *field* to carry a name. Without
    /// this, a company's page would label its incoming edges with the forward
    /// field's name and read "Company: Acme" on Acme's own page. The panel falls
    /// back to the target object's `plural` when it is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation_inverse_label: Option<String>,
    /// `currency`: ISO-4217 code. Defaults to `USD` when absent. Amounts are always
    /// integer cents; this only decides the symbol and grouping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency_code: Option<String>,
    /// `number` / `percent`: decimal places the UI renders. Storage is unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision: Option<u8>,
    /// `rating`: the top of the scale. Defaults to 5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rating: Option<u8>,
    /// Applied by [`crate::store::CrmStore::create_record`] when the value bag omits
    /// this field. Already in normalized form — the store validates a default the
    /// same way it validates user input, at field-creation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_value: Option<Value>,
    /// `text` / `long_text`: soft length hint the panel enforces. Not a storage
    /// bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
}

impl FieldConfig {
    pub const DEFAULT_CURRENCY: &'static str = "USD";
    pub const DEFAULT_MAX_RATING: u8 = 5;

    /// Options sorted by position, which is the order every renderer and the board
    /// grouping use.
    pub fn sorted_options(&self) -> Vec<SelectOption> {
        let mut options = self.options.clone();
        options.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        options
    }

    pub fn option(&self, id: &str) -> Option<&SelectOption> {
        self.options.iter().find(|o| o.id == id)
    }

    /// Match an option by id first, then case-insensitively by label. The label leg
    /// exists for CSV import and agent tool calls, where the input is "Proposal",
    /// not `opt_deal_stage_proposal`.
    pub fn resolve_option(&self, raw: &str) -> Option<&SelectOption> {
        let trimmed = raw.trim();
        self.options
            .iter()
            .find(|o| o.id == trimmed)
            .or_else(|| self.options.iter().find(|o| o.label.eq_ignore_ascii_case(trimmed)))
    }

    pub fn currency(&self) -> &str {
        self.currency_code
            .as_deref()
            .filter(|c| !c.trim().is_empty())
            .unwrap_or(Self::DEFAULT_CURRENCY)
    }

    pub fn max_rating(&self) -> u8 {
        self.max_rating.filter(|m| *m > 0).unwrap_or(Self::DEFAULT_MAX_RATING)
    }

    /// Tolerant decode of the stored JSON. A config that fails to parse degrades to
    /// the default rather than failing the whole field list — a field with no
    /// options renders as a plain input, which is recoverable; a blank schema is
    /// not.
    pub fn decode(raw: &str) -> Self {
        serde_json::from_str(raw).unwrap_or_default()
    }

    pub fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// One typed attribute on an object — or, when [`Field::list_id`] is set, on a
/// LIST.
///
/// One table and one type for both, deliberately. A list's extra fields ("what stage
/// is this deal at *inside this sales list*") are the same thing structurally: a
/// slug, a type, a config, a validator. Splitting them would duplicate all seventeen
/// type validators, and the duplicate would drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub id: String,
    /// Always the OBJECT this field's values live on — including for a list field,
    /// where it is the list's own object. That keeps "which object is this field
    /// about" answerable without a join.
    pub object_id: String,
    /// `Some` for a LIST-specific field, whose values live in
    /// [`ListEntry::values`], not in [`Record::values`]. `None` for an object field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_id: Option<String>,
    /// Immutable after creation. See the module docs.
    pub slug: String,
    pub name: String,
    pub field_type: FieldType,
    #[serde(default)]
    pub config: FieldConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A create/replace with this field empty is rejected. A partial (merge) update
    /// that does not mention it is not — otherwise no PATCH could ever touch one
    /// field of a record.
    pub is_required: bool,
    /// No two live records on this object may share a non-empty value. Enforced in
    /// the write transaction, not by a SQL index — see [`crate::store`].
    pub is_unique: bool,
    /// Seeded fields the product depends on. `is_system` fields cannot be deleted
    /// and their type cannot change; their `name`, `config` and `position` can.
    pub is_system: bool,
    pub position: i64,
    pub created_at: String,
    pub updated_at: String,
}

impl Field {
    /// The JSON path this field's value lives at inside a value bag. Safe to
    /// interpolate BECAUSE [`is_valid_slug`] closed the alphabet — see its docs.
    pub fn json_path(&self) -> String {
        format!("$.{}", self.slug)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateFieldRequest {
    pub slug: String,
    pub name: String,
    pub field_type: FieldType,
    #[serde(default)]
    pub config: FieldConfig,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub is_required: bool,
    #[serde(default)]
    pub is_unique: bool,
    /// Appended to the end when absent.
    #[serde(default)]
    pub position: Option<i64>,
}

impl Default for FieldType {
    fn default() -> Self {
        Self::Text
    }
}

/// Note the absent `slug` and `field_type`: both are immutable. See the module docs
/// for why a slug rename is not a rename but a migration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateFieldRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub config: Option<FieldConfig>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub is_required: Option<bool>,
    #[serde(default)]
    pub is_unique: Option<bool>,
    #[serde(default)]
    pub position: Option<i64>,
}

/// `POST …/fields/reorder`, `POST …/entries/reorder`. The listed ids get positions
/// `0..n` in the order given; anything omitted keeps its relative order after them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReorderRequest {
    pub ids: Vec<String>,
}

// ── Validation ─────────────────────────────────────────────────────────────────

/// Why one field's value was rejected. Machine-readable so the panel can render a
/// specific affordance ("pick from the list") rather than a sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCode {
    /// Required field absent or empty.
    Required,
    /// Present but the wrong shape for the type.
    Invalid,
    /// Names a field this object does not have.
    UnknownField,
    /// Not one of the field's options.
    UnknownOption,
    /// Collides with another live record on a `is_unique` field.
    NotUnique,
    /// A relation target that does not exist, is deleted, or is on the wrong object.
    BadRelationTarget,
    /// Out of the type's allowed range (a rating above `max_rating`, a percent
    /// outside 0..=100).
    OutOfRange,
}

/// One field-level rejection. Collected, never thrown one at a time — see
/// [`ValidatedValues`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldValidationError {
    /// Empty for [`ValidationCode::UnknownField`], where by definition there is no
    /// field to point at.
    #[serde(default)]
    pub field_id: String,
    pub field_slug: String,
    pub code: ValidationCode,
    pub message: String,
}

impl FieldValidationError {
    /// The general-purpose constructor: an [`ValidationCode::Invalid`] rejection.
    pub fn new(
        field_id: impl Into<String>,
        field_slug: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            field_id: field_id.into(),
            field_slug: field_slug.into(),
            code: ValidationCode::Invalid,
            message: message.into(),
        }
    }

    pub fn coded(
        field_id: impl Into<String>,
        field_slug: impl Into<String>,
        code: ValidationCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            field_id: field_id.into(),
            field_slug: field_slug.into(),
            code,
            message: message.into(),
        }
    }

    pub fn unknown_field(slug: impl Into<String>) -> Self {
        let slug = slug.into();
        Self {
            field_id: String::new(),
            message: format!("no field \"{slug}\" on this object"),
            field_slug: slug,
            code: ValidationCode::UnknownField,
        }
    }
}

/// The result of validating a value bag: the NORMALIZED values plus every reason a
/// field was rejected.
///
/// Non-throwing on purpose. A CSV import needs to report "row 41 has two bad cells"
/// for 10 000 rows without unwinding once per row, and a record form needs to light
/// up every bad cell at once. The write paths call [`Self::is_ok`] and turn the
/// errors into a 422; the import path keeps them per row.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValidatedValues {
    /// Only the fields that validated. Values are canonical: cents for currency,
    /// `YYYY-MM-DD` for date, option **ids** for select/status, arrays for
    /// relation/multi-select.
    pub values: ValueBag,
    pub errors: Vec<FieldValidationError>,
}

impl ValidatedValues {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

// ── Records ────────────────────────────────────────────────────────────────────

/// One row of one object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub object_id: String,
    /// Denormalized display name, recomputed on every write from the object's
    /// `title_field_id`. Stored rather than derived so a list of 500 records sorts
    /// and renders without loading the schema, and so FTS has something to rank.
    pub title: String,
    /// Field slug → canonical value. See [`ValidatedValues`] for what "canonical"
    /// means per type.
    #[serde(default)]
    pub values: ValueBag,
    /// Soft delete. A non-`None` value hides the record from every default query;
    /// the row survives so its activities, links and list memberships are still
    /// explicable, and so an accidental delete is one `POST …/restore` away.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
    /// Free-form actor string (a Ryu user handle, `import:imp_…`, `agent`). Harbor
    /// does not own identity, so it does not validate this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// One field that actually moved during an update. The unit of `record.updated`'s
/// payload and of the `field_change` timeline entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldChange {
    pub field_id: String,
    pub field_slug: String,
    pub field_name: String,
    /// `Null` when the field was previously unset.
    pub from: Value,
    /// `Null` when the field was cleared.
    pub to: Value,
}

/// What [`crate::store::CrmStore::update_record`] returns: the new row plus the diff
/// that produced it.
///
/// The diff is returned rather than recomputed by the caller because the store is
/// the only place that has both bags in normalized form at the same time. An empty
/// `changed` means the PATCH was a no-op and the caller must NOT emit
/// `record.updated` — see [`crate::events::record_updated`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordUpdate {
    pub record: Record,
    pub changed: Vec<FieldChange>,
    /// Populated when a `status`-typed field moved, so the caller can raise
    /// `deal.stage_changed` without re-reading the schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_change: Option<StageChange>,
}

/// A `status` field transition, extracted from a [`RecordUpdate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageChange {
    pub field_id: String,
    pub field_slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_label: Option<String>,
}

/// Whether an update replaces the bag or merges into it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateMode {
    /// Only the mentioned slugs change; a slug mapped to `null` is CLEARED, and an
    /// absent slug is untouched. The default, and what a cell edit sends.
    #[default]
    Merge,
    /// The bag becomes exactly what was sent — every absent field is cleared, and
    /// required fields are enforced. What a full-form save sends.
    Replace,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateRecordRequest {
    #[serde(default)]
    pub values: ValueBag,
    #[serde(default)]
    pub created_by: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateRecordRequest {
    #[serde(default)]
    pub values: ValueBag,
    #[serde(default)]
    pub mode: UpdateMode,
}

/// Everything the record drawer renders, in one request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordDetail {
    pub record: Record,
    pub object: Object,
    pub fields: Vec<Field>,
    /// Both directions — see [`RecordLinkView`].
    pub links: Vec<RecordLinkView>,
    /// Newest first, capped at [`RecordDetail::TIMELINE_LIMIT`]. The full timeline
    /// is a separate paginated call.
    pub activities: Vec<Activity>,
    /// Which curated lists this record sits in.
    pub lists: Vec<ListMembership>,
}

impl RecordDetail {
    pub const TIMELINE_LIMIT: usize = 25;
}

// ── Relations ──────────────────────────────────────────────────────────────────

/// One materialised relation edge.
///
/// **One row per edge, not two.** "Both sides queryable" is achieved by indexing
/// `source_record_id` and `target_record_id` separately and reading the table with
/// an `OR`, not by writing a mirror row. The mirror is the tempting design and it is
/// wrong: two rows can diverge (a partial delete, a crash between the two writes)
/// and nothing in the schema says which one is authoritative. One row cannot
/// disagree with itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordLink {
    pub id: String,
    /// The `relation` field that owns this edge. Always belongs to the SOURCE
    /// object.
    pub field_id: String,
    pub source_record_id: String,
    pub source_object_id: String,
    pub target_record_id: String,
    pub target_object_id: String,
    pub created_at: String,
}

/// Which end of an edge a record is on, from that record's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkDirection {
    /// This record holds the relation field; `record` is what it points AT.
    Outgoing,
    /// Something else points at this record; `record` is the pointer.
    Incoming,
}

/// An edge as seen FROM one record, with the other end resolved for rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordLinkView {
    pub link_id: String,
    pub field_id: String,
    /// The forward field's name for [`LinkDirection::Outgoing`]; the field's
    /// [`FieldConfig::relation_inverse_label`] (falling back to the source object's
    /// `plural`) for [`LinkDirection::Incoming`]. Without the fallback an incoming
    /// edge on Acme's page reads "Company: Jane Doe", which is backwards.
    pub label: String,
    pub direction: LinkDirection,
    /// The record at the OTHER end.
    pub record_id: String,
    pub object_id: String,
    pub title: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinkRequest {
    pub field_id: String,
    pub target_record_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RelatedQuery {
    /// Restrict to one relation field. Absent = every edge in both directions.
    #[serde(default)]
    pub field_id: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

// ── Filters, sorts, pagination ─────────────────────────────────────────────────

/// A recursive filter tree. `{"type":"and","filters":[…]}` /
/// `{"type":"condition","field_id":"fld_…","op":"eq","value":"x"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ViewFilter {
    And { filters: Vec<ViewFilter> },
    Or { filters: Vec<ViewFilter> },
    Not { filter: Box<ViewFilter> },
    Condition(FilterCondition),
}

impl ViewFilter {
    /// An empty `And` — matches everything. The identity the query builder uses when
    /// a view has no filter, so there is no `Option` branch in the SQL assembly.
    pub fn all() -> Self {
        Self::And {
            filters: Vec::new(),
        }
    }

    /// Whether this tree constrains anything. An empty `And`/`Or` node is treated as
    /// "no filter" rather than "matches nothing", because a UI that has not finished
    /// building a filter must not blank the table.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::And { filters } | Self::Or { filters } => {
                filters.is_empty() || filters.iter().all(Self::is_empty)
            }
            Self::Not { filter } => filter.is_empty(),
            Self::Condition(_) => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterCondition {
    /// A field id, a field slug, or one of the intrinsic keys `title`,
    /// `created_at`, `updated_at` (see [`ViewSort::is_intrinsic`]). The store
    /// resolves all three.
    pub field_id: String,
    pub op: FilterOperator,
    /// Interpreted per operator: a scalar for the comparisons, an array for
    /// `is_any_of` / `is_none_of` / `between`, ignored for `is_empty` /
    /// `is_not_empty` / `is_true` / `is_false`.
    #[serde(default)]
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOperator {
    Eq,
    NotEq,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    Gt,
    Gte,
    Lt,
    Lte,
    /// Inclusive both ends; `value` is a two-element array.
    Between,
    IsEmpty,
    IsNotEmpty,
    /// Set membership. For a multi-valued field (`multi_select`, `relation`) this is
    /// "intersects", which is the only reading that is useful.
    IsAnyOf,
    IsNoneOf,
    IsTrue,
    IsFalse,
}

impl FilterOperator {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::NotEq => "not_eq",
            Self::Contains => "contains",
            Self::NotContains => "not_contains",
            Self::StartsWith => "starts_with",
            Self::EndsWith => "ends_with",
            Self::Gt => "gt",
            Self::Gte => "gte",
            Self::Lt => "lt",
            Self::Lte => "lte",
            Self::Between => "between",
            Self::IsEmpty => "is_empty",
            Self::IsNotEmpty => "is_not_empty",
            Self::IsAnyOf => "is_any_of",
            Self::IsNoneOf => "is_none_of",
            Self::IsTrue => "is_true",
            Self::IsFalse => "is_false",
        }
    }

    /// Whether the operator reads `value` at all.
    pub const fn needs_value(self) -> bool {
        !matches!(
            self,
            Self::IsEmpty | Self::IsNotEmpty | Self::IsTrue | Self::IsFalse
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    #[default]
    Asc,
    Desc,
}

impl SortDirection {
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSort {
    /// A field id, a field slug, or an intrinsic key.
    pub field_id: String,
    #[serde(default)]
    pub direction: SortDirection,
}

impl ViewSort {
    /// The three keys that name a real `records` COLUMN rather than a value-bag
    /// entry. [`RESERVED_SLUGS`] forbids a user field from taking one, so the
    /// mapping is unambiguous.
    pub const INTRINSIC: [&'static str; 3] = ["title", "created_at", "updated_at"];

    pub fn is_intrinsic(key: &str) -> bool {
        Self::INTRINSIC.contains(&key)
    }

    pub fn asc(field_id: impl Into<String>) -> Self {
        Self {
            field_id: field_id.into(),
            direction: SortDirection::Asc,
        }
    }

    pub fn desc(field_id: impl Into<String>) -> Self {
        Self {
            field_id: field_id.into(),
            direction: SortDirection::Desc,
        }
    }
}

/// The ONE pagination envelope every list endpoint in this app returns.
///
/// `total` is the count BEFORE `limit`/`offset`, which costs a second query and is
/// worth it: without it the panel cannot render "1–50 of 812" and cannot size a
/// scrollbar. `has_more` is derived rather than inferred by the client, because
/// `offset + items.len() < total` is exactly the arithmetic a client gets wrong.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub limit: usize,
    pub offset: usize,
    pub has_more: bool,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, total: i64, limit: usize, offset: usize) -> Self {
        let has_more = (offset as i64) + (items.len() as i64) < total;
        Self {
            items,
            total,
            limit,
            offset,
            has_more,
        }
    }

    pub fn empty(limit: usize, offset: usize) -> Self {
        Self {
            items: Vec::new(),
            total: 0,
            limit,
            offset,
            has_more: false,
        }
    }
}

pub type RecordPage = Page<Record>;
pub type ActivityPage = Page<Activity>;
pub type ListEntryPage = Page<ListEntryView>;

/// The full record query. Also the body of `POST …/records/query`, minus
/// `object_id`, which comes from the path.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecordQuery {
    /// Object id or slug.
    #[serde(default)]
    pub object_id: String,
    #[serde(default)]
    pub filter: Option<ViewFilter>,
    #[serde(default)]
    pub sorts: Vec<ViewSort>,
    /// Full-text pre-filter. When present the candidate set comes from FTS5 and the
    /// filter tree narrows it, so `search` + `filter` compose rather than compete.
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    /// Include soft-deleted rows. Default `false`; the trash view sets it.
    #[serde(default)]
    pub include_deleted: bool,
    /// Restrict to records that are entries of this list.
    #[serde(default)]
    pub list_id: Option<String>,
    /// Restrict to an explicit id set. Used by the merge preview and by
    /// `related_records`.
    #[serde(default)]
    pub record_ids: Option<Vec<String>>,
}

// ── Views ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewKind {
    #[default]
    Table,
    /// Kanban. Requires [`View::group_by_field_id`] to name a `select` or `status`
    /// field; a board without one degrades to a single "All" column rather than
    /// erroring, so a mid-edit view never 500s.
    Board,
    /// A dense one-line-per-record reading list.
    List,
}

impl ViewKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::Board => "board",
            Self::List => "list",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "board" => Self::Board,
            "list" => Self::List,
            _ => Self::Table,
        }
    }
}

/// A saved view on one object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub id: String,
    pub object_id: String,
    pub name: String,
    pub kind: ViewKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<ViewFilter>,
    #[serde(default)]
    pub sorts: Vec<ViewSort>,
    /// Field ids, in render order. Empty means "every field in `position` order",
    /// which is what a freshly created view wants.
    #[serde(default)]
    pub visible_field_ids: Vec<String>,
    /// Board grouping. Ignored for other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by_field_id: Option<String>,
    /// Exactly one view per object carries this. Opening the object opens it.
    pub is_default: bool,
    pub position: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateViewRequest {
    pub name: String,
    #[serde(default)]
    pub kind: ViewKind,
    #[serde(default)]
    pub filter: Option<ViewFilter>,
    #[serde(default)]
    pub sorts: Vec<ViewSort>,
    #[serde(default)]
    pub visible_field_ids: Vec<String>,
    #[serde(default)]
    pub group_by_field_id: Option<String>,
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateViewRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<ViewKind>,
    /// `Some(None)` is not expressible in JSON, so clearing a filter is
    /// `{"filter": {"type":"and","filters":[]}}` — the identity tree.
    #[serde(default)]
    pub filter: Option<ViewFilter>,
    #[serde(default)]
    pub sorts: Option<Vec<ViewSort>>,
    #[serde(default)]
    pub visible_field_ids: Option<Vec<String>>,
    #[serde(default)]
    pub group_by_field_id: Option<String>,
    #[serde(default)]
    pub position: Option<i64>,
}

/// Per-request overrides layered on a saved view — the "I filtered the table but did
/// not save it" case.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ViewQueryOverrides {
    /// ANDed WITH the view's own filter, never replacing it: a saved view named
    /// "Open deals" that a search box could silently widen is a lie.
    #[serde(default)]
    pub filter: Option<ViewFilter>,
    /// REPLACES the view's sorts when present (clicking a column header re-sorts).
    #[serde(default)]
    pub sorts: Option<Vec<ViewSort>>,
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub include_deleted: bool,
}

/// One kanban column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardGroup {
    /// The option id, or `null` for the "no value" column — which always exists and
    /// always sorts last, because records with no stage are the ones that need
    /// attention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub position: i64,
    /// Total in this column, not just the loaded page.
    pub total: i64,
    /// Sum of the board's currency field over the whole column, in cents. `None`
    /// when the object has no currency field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_cents: Option<i64>,
    pub records: Vec<Record>,
}

/// `POST /api/crm/views/{view_id}/run` — everything the table/board needs to paint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewResult {
    pub view: View,
    /// The fields to render as columns, already resolved from
    /// `visible_field_ids` (or all of them, in position order, when it is empty).
    pub fields: Vec<Field>,
    /// Flat page. Populated for every kind — a board also fills `groups`, and the
    /// flat page then holds the same records so a client that only understands
    /// tables still works.
    pub page: RecordPage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<BoardGroup>>,
}

// ── Lists ──────────────────────────────────────────────────────────────────────

/// A curated subset of one object's records, with its own extra fields.
///
/// The Attio idea, and the reason it is not just a saved view: a list is
/// *membership*, chosen by a human, and it carries per-membership data ("what stage
/// is this deal at inside THIS sales cycle") that has no meaning on the record
/// itself. A saved view is a query; a list is a set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct List {
    pub id: String,
    /// The object whose records this list may contain. A list is single-object;
    /// mixing objects would make its extra fields untypeable.
    pub object_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub position: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateListRequest {
    pub object_id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateListRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub position: Option<i64>,
}

/// One record's membership in one list, plus its list-specific values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListEntry {
    pub id: String,
    pub list_id: String,
    pub record_id: String,
    /// Keyed by LIST-field slug (a [`Field`] with `list_id` set), never by object
    /// field slug. The two namespaces are separate: a list may have its own `stage`
    /// that has nothing to do with the deal's own `stage`.
    #[serde(default)]
    pub values: ValueBag,
    pub position: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// An entry with its record resolved, which is what the list table renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListEntryView {
    #[serde(flatten)]
    pub entry: ListEntry,
    pub record: Record,
}

/// Shown on a record's drawer: "this deal is in 2 lists".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListMembership {
    pub list_id: String,
    pub list_name: String,
    pub entry_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AddListEntryRequest {
    pub record_id: String,
    #[serde(default)]
    pub values: ValueBag,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateListEntryRequest {
    #[serde(default)]
    pub values: ValueBag,
    #[serde(default)]
    pub mode: UpdateMode,
}

/// Query over one list's entries. Filters and sorts may reference BOTH the record's
/// own fields and the list's extra fields; the store knows which is which from the
/// field's `list_id`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListEntryQuery {
    #[serde(default)]
    pub list_id: String,
    #[serde(default)]
    pub filter: Option<ViewFilter>,
    #[serde(default)]
    pub sorts: Vec<ViewSort>,
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

// ── Activities ─────────────────────────────────────────────────────────────────

/// The six things that can appear on a record's timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Note,
    /// The only kind that uses `assignee` / `due_at` / `completed_at`.
    Task,
    Call,
    Meeting,
    /// Written automatically by the store on every record update. Not user-creatable
    /// — a hand-forged audit entry is worse than none.
    FieldChange,
    /// Written automatically when a `status` field moves. Redundant with
    /// `FieldChange` on purpose: the pipeline/funnel report reads only these, and
    /// scanning every field change to find the stage ones is the query that makes
    /// reporting slow.
    StageChange,
}

impl ActivityKind {
    pub const ALL: [ActivityKind; 6] = [
        ActivityKind::Note,
        ActivityKind::Task,
        ActivityKind::Call,
        ActivityKind::Meeting,
        ActivityKind::FieldChange,
        ActivityKind::StageChange,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Task => "task",
            Self::Call => "call",
            Self::Meeting => "meeting",
            Self::FieldChange => "field_change",
            Self::StageChange => "stage_change",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Tolerant decode. An unknown kind reads as a note, which renders as plain text
    /// rather than blanking the timeline.
    pub fn from_db(s: &str) -> Self {
        Self::parse(s).unwrap_or(Self::Note)
    }

    /// Whether a user may create this kind directly. The two automatic kinds may
    /// not — see their docs.
    pub const fn is_user_authored(self) -> bool {
        matches!(self, Self::Note | Self::Task | Self::Call | Self::Meeting)
    }
}

/// One timeline entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub id: String,
    /// `None` for a standalone task — one created from the task list with no record
    /// attached. Everything else hangs off a record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    /// Denormalized from the record so the global feed can filter by object without
    /// a join. Set even when `record_id` is `None` if the caller named an object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_id: Option<String>,
    pub kind: ActivityKind,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// `field_change` / `stage_change`: which field moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_id: Option<String>,
    /// `field_change` / `stage_change`: the previous value, already normalized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_value: Option<Value>,
    /// Tasks only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Tasks only. RFC-3339; the due sweep range-scans this column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<String>,
    /// Tasks only. `Some` ⇒ done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    /// When `task.due` was raised for this task. Set by the claim, never by a
    /// handler — it is the idempotency stamp that stops a restart re-announcing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_notified_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Free-form extras (a call's duration, a meeting's attendees). Opaque to the
    /// store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    pub created_at: String,
    pub updated_at: String,
}

impl Activity {
    /// An open task is one that is a task, has not been completed, and is not
    /// soft-deleted with its record.
    pub fn is_open_task(&self) -> bool {
        self.kind == ActivityKind::Task && self.completed_at.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateActivityRequest {
    /// Absent for a standalone task. Set from the path on
    /// `POST /records/{id}/activities`.
    #[serde(default)]
    pub record_id: Option<String>,
    pub kind: ActivityKind,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub due_at: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

impl Default for CreateActivityRequest {
    fn default() -> Self {
        Self {
            record_id: None,
            kind: ActivityKind::Note,
            title: String::new(),
            body: None,
            assignee: None,
            due_at: None,
            author: None,
            metadata: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateActivityRequest {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub due_at: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    /// `Some(true)` completes, `Some(false)` reopens. Absent leaves it alone.
    #[serde(default)]
    pub completed: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompleteTaskRequest {
    #[serde(default = "default_true")]
    pub completed: bool,
}

fn default_true() -> bool {
    true
}

/// The timeline query. Also the query string of `GET /api/crm/activities`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActivityQuery {
    #[serde(default)]
    pub record_id: Option<String>,
    /// Object id or slug — the global feed filtered to one object.
    #[serde(default)]
    pub object_id: Option<String>,
    /// Empty = every kind.
    #[serde(default)]
    pub kinds: Vec<ActivityKind>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub search: Option<String>,
    /// RFC-3339 bounds on `created_at`.
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// Which slice of the task list to return.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskFilter {
    /// Not completed. The default, and what the panel's task tab shows.
    #[default]
    Open,
    Completed,
    /// Not completed and `due_at` in the past.
    Overdue,
    All,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskQuery {
    #[serde(default)]
    pub filter: TaskFilter,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub record_id: Option<String>,
    #[serde(default)]
    pub object_id: Option<String>,
    /// Inclusive RFC-3339 bounds on `due_at`.
    #[serde(default)]
    pub due_before: Option<String>,
    #[serde(default)]
    pub due_after: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

// ── Merge ──────────────────────────────────────────────────────────────────────

/// Two or more records the dedupe scan believes are the same thing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeCandidate {
    /// Oldest first, so `record_ids[0]` is the natural survivor suggestion.
    pub record_ids: Vec<String>,
    /// The field whose value they share.
    pub field_id: String,
    pub field_slug: String,
    /// The shared value, normalized (trimmed, lowercased for email/text).
    pub value: String,
    /// 0.0–1.0. Exact normalized equality scores 1.0; this exists so a future fuzzy
    /// matcher has somewhere to put its confidence without a contract change.
    pub score: f64,
    /// Titles in the same order as `record_ids`, so the picker renders without a
    /// second fetch.
    pub titles: Vec<String>,
}

/// Where one field's surviving value comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum MergeSource {
    /// Keep what the survivor already has. The default for every field the plan does
    /// not mention.
    Survivor,
    /// Take it from a specific loser.
    Loser { record_id: String },
    /// Take an explicit value the user typed in the merge dialog.
    Value { value: Value },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergeFieldResolution {
    pub field_id: String,
    #[serde(flatten)]
    pub source: MergeSource,
}

/// The merge request. Field-level, not "last write wins": a merge that silently
/// discarded the loser's phone number because the survivor had a blank one is the
/// single most-complained-about behaviour in every CRM that does it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MergePlan {
    pub survivor_id: String,
    pub loser_ids: Vec<String>,
    /// Fields the plan resolves explicitly. Anything omitted defaults to
    /// [`MergeSource::Survivor`] when the survivor's value is non-empty, and to the
    /// first non-empty loser value otherwise — so the common "fill the blanks" case
    /// needs no resolutions at all.
    #[serde(default)]
    pub resolutions: Vec<MergeFieldResolution>,
    /// Soft-delete the losers instead of hard-deleting them. Default `true`: an
    /// unrecoverable merge is a support ticket.
    #[serde(default = "default_true")]
    pub soft_delete_losers: bool,
}

/// One field where the survivor and a loser disagree, both values non-empty. The
/// merge dialog renders exactly these as radio rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeConflict {
    pub field_id: String,
    pub field_slug: String,
    pub field_name: String,
    pub survivor_value: Value,
    /// One entry per loser that has a differing non-empty value.
    pub loser_values: Vec<MergeLoserValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeLoserValue {
    pub record_id: String,
    pub title: String,
    pub value: Value,
}

/// Dry run of [`MergePlan`]. Nothing is written.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergePreview {
    pub survivor: Record,
    pub losers: Vec<Record>,
    /// What the survivor's bag would become.
    pub resolved_values: ValueBag,
    pub conflicts: Vec<MergeConflict>,
    /// How much history follows the losers onto the survivor.
    pub activity_count: i64,
    pub link_count: i64,
    pub list_entry_count: i64,
}

/// What actually happened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeOutcome {
    pub survivor: Record,
    pub merged_record_ids: Vec<String>,
    pub moved_activities: i64,
    pub moved_links: i64,
    pub moved_list_entries: i64,
    /// The fields the merge actually changed on the survivor, so the caller can
    /// raise `record.updated` with a real diff.
    pub changed: Vec<FieldChange>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DuplicateScanRequest {
    /// Fields to match on. Empty = the object's `is_unique` fields, then its email
    /// fields, then its title field — in that order, first non-empty set wins.
    #[serde(default)]
    pub field_ids: Vec<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateScanResponse {
    pub candidates: Vec<MergeCandidate>,
    /// Which fields the scan actually used, since an empty request means "decide for
    /// me" and the UI must be able to say what it decided.
    pub field_ids: Vec<String>,
}

// ── CSV import ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportStatus {
    /// Uploaded, columns inferred, not yet mapped.
    #[default]
    Draft,
    /// Mapping saved and a dry run computed.
    Previewed,
    Applied,
    /// The apply failed outright (not the same as rows failing inside a successful
    /// apply, which land in [`ImportResult::errors`]).
    Failed,
}

impl ImportStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Previewed => "previewed",
            Self::Applied => "applied",
            Self::Failed => "failed",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "previewed" => Self::Previewed,
            "applied" => Self::Applied,
            "failed" => Self::Failed,
            _ => Self::Draft,
        }
    }
}

/// What to do when an incoming row matches an existing record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DedupeStrategy {
    /// Import everything as new records. Honest, and occasionally what you want.
    CreateAlways,
    /// Leave the existing record untouched.
    #[default]
    Skip,
    /// Overwrite every mapped field on the existing record.
    Update,
    /// Write only the mapped fields that are currently EMPTY on the existing record.
    /// The safe default for enrichment files.
    FillBlanks,
}

impl DedupeStrategy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CreateAlways => "create_always",
            Self::Skip => "skip",
            Self::Update => "update",
            Self::FillBlanks => "fill_blanks",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "create_always" => Self::CreateAlways,
            "update" => Self::Update,
            "fill_blanks" => Self::FillBlanks,
            _ => Self::Skip,
        }
    }
}

/// One CSV column as the uploader sees it, before mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportColumn {
    pub index: usize,
    /// The header cell, or `Column 3` when the file has no header row.
    pub name: String,
    /// Up to [`ImportColumn::SAMPLE_ROWS`] non-empty values, so the mapping UI can
    /// show what is actually in the column.
    pub samples: Vec<String>,
    /// The field this column probably maps to — matched by slug, then by
    /// case-insensitive field name, then by a slugified header. A suggestion only;
    /// nothing is imported until the mapping is saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_field_id: Option<String>,
}

impl ImportColumn {
    pub const SAMPLE_ROWS: usize = 3;
}

/// One column's decided destination.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportMapping {
    pub column_index: usize,
    /// `None` ⇒ the column is ignored. A field id or slug.
    #[serde(default)]
    pub field_id: Option<String>,
}

/// How the import decides a row is a duplicate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportDedupe {
    /// Fields to match on, ANDed. Empty ⇒ no matching, every row creates.
    #[serde(default)]
    pub match_field_ids: Vec<String>,
    #[serde(default)]
    pub strategy: DedupeStrategy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportAction {
    Create,
    Update,
    Skip,
    /// The row failed validation and will not be written.
    Error,
}

/// What one row would do (preview) or did (apply).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRowPlan {
    /// 0-based index into the DATA rows (the header, if any, is not row 0).
    pub row_index: usize,
    pub action: ImportAction,
    /// The record this row matched, for `Update` / `Skip`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    /// Normalized values this row would write.
    #[serde(default)]
    pub values: ValueBag,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<FieldValidationError>,
}

/// A mapped field where the incoming value differs from what the matched record
/// already has. Surfaced in the preview so nobody discovers an overwrite afterwards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportConflict {
    pub row_index: usize,
    pub record_id: String,
    pub field_id: String,
    pub field_slug: String,
    pub existing: Value,
    pub incoming: Value,
}

/// The dry run. Counts over EVERY row; `samples`/`conflicts` are capped.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportPreview {
    pub total_rows: usize,
    pub create_count: usize,
    pub update_count: usize,
    pub skip_count: usize,
    pub error_count: usize,
    /// The first [`ImportPreview::SAMPLE_LIMIT`] row plans, plus every row plan whose
    /// action is `Error` up to the same cap — a preview that hides the failures is
    /// worse than no preview.
    #[serde(default)]
    pub samples: Vec<ImportRowPlan>,
    #[serde(default)]
    pub conflicts: Vec<ImportConflict>,
    /// Columns with no mapping, by header name. The commonest import mistake is
    /// forgetting one.
    #[serde(default)]
    pub unmapped_columns: Vec<String>,
    /// Whether the counts and samples cover the whole file.
    #[serde(default)]
    pub truncated: bool,
}

impl ImportPreview {
    pub const SAMPLE_LIMIT: usize = 25;
    pub const CONFLICT_LIMIT: usize = 100;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRowError {
    pub row_index: usize,
    pub errors: Vec<FieldValidationError>,
}

/// What the apply actually did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportResult {
    pub created: usize,
    pub updated: usize,
    pub skipped: usize,
    pub failed: usize,
    /// Ids of records CREATED, so the caller can raise one `record.created` each.
    #[serde(default)]
    pub created_record_ids: Vec<String>,
    /// Ids of records UPDATED, for `record.updated`.
    #[serde(default)]
    pub updated_record_ids: Vec<String>,
    /// Capped at [`ImportResult::ERROR_LIMIT`]; `failed` is the true count.
    #[serde(default)]
    pub errors: Vec<ImportRowError>,
}

impl ImportResult {
    pub const ERROR_LIMIT: usize = 200;
}

/// One import, from upload to apply. The raw CSV lives in the row (not on disk and
/// not in memory between requests) so preview and apply see byte-identical input —
/// a preview computed over different bytes than the apply is not a preview.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportJob {
    pub id: String,
    pub object_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub status: ImportStatus,
    /// The single-byte delimiter, as a string (`,`, `;`, `\t`).
    pub delimiter: String,
    pub has_header: bool,
    /// DATA rows, excluding the header.
    pub row_count: usize,
    pub columns: Vec<ImportColumn>,
    #[serde(default)]
    pub mappings: Vec<ImportMapping>,
    #[serde(default)]
    pub dedupe: ImportDedupe,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<ImportPreview>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ImportResult>,
    /// Set when `status` is [`ImportStatus::Failed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateImportRequest {
    /// Object id or slug.
    pub object_id: String,
    #[serde(default)]
    pub filename: Option<String>,
    /// The whole file as text. JSON string, not multipart: the panel reads the file
    /// with the FileReader API and the ext-proxy forwards a JSON body unchanged,
    /// whereas multipart through two hops is a stream-rewriting problem nobody
    /// needs. Bounded by `Config::max_import_bytes`.
    pub csv: String,
    /// Sniffed from the first line when absent.
    #[serde(default)]
    pub delimiter: Option<String>,
    /// Inferred when absent: a first row whose cells are all non-numeric and unique
    /// is a header.
    #[serde(default)]
    pub has_header: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetImportMappingRequest {
    pub mappings: Vec<ImportMapping>,
    #[serde(default)]
    pub dedupe: ImportDedupe,
}

// ── Search & reports ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchQuery {
    #[serde(default, alias = "q")]
    pub query: String,
    /// Restrict to these objects (ids or slugs). Empty = everything.
    #[serde(default)]
    pub object_ids: Vec<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub record_id: String,
    pub object_id: String,
    pub object_slug: String,
    pub title: String,
    /// FTS5 `snippet()` output with `<mark>` delimiters around the match.
    pub snippet: String,
    /// FTS5 `bm25()`. LOWER is better; the list is already sorted, so a client
    /// should not re-sort on it.
    pub rank: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub hits: Vec<SearchHit>,
    pub total: i64,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipelineRequest {
    /// Object id or slug. Defaults to `deal`.
    #[serde(default)]
    pub object_id: Option<String>,
    /// The `status` field to bucket by. Defaults to the object's first status field
    /// by position.
    #[serde(default)]
    pub field_id: Option<String>,
    /// The `currency` field to sum. Defaults to the object's first currency field;
    /// `None` when it has none, in which case the report is counts only.
    #[serde(default)]
    pub value_field_id: Option<String>,
    #[serde(default)]
    pub filter: Option<ViewFilter>,
    /// Include won/lost stages. Default `true`; a "what is open" board sets it
    /// false.
    #[serde(default = "default_true")]
    pub include_closed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStage {
    pub option_id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub position: i64,
    pub is_won: bool,
    pub is_lost: bool,
    pub record_count: i64,
    /// Integer cents. `0` when the report has no value field.
    pub value_cents: i64,
    /// This stage's share of `total_records`, 0.0–1.0.
    pub share: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineReport {
    pub object_id: String,
    pub field_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_field_id: Option<String>,
    pub currency_code: String,
    pub total_records: i64,
    pub total_value_cents: i64,
    /// Records whose stage field is unset. Counted separately rather than dropped —
    /// a pipeline report that quietly excludes rows is how forecasts go wrong.
    pub unassigned_count: i64,
    pub stages: Vec<PipelineStage>,
    pub won_count: i64,
    pub won_value_cents: i64,
    pub lost_count: i64,
    pub lost_value_cents: i64,
    /// `won / (won + lost)`, 0.0–1.0; `0.0` when nothing has closed.
    pub win_rate: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FunnelRequest {
    #[serde(default)]
    pub object_id: Option<String>,
    #[serde(default)]
    pub field_id: Option<String>,
    /// RFC-3339 window over the stage-change activities. Absent = all time.
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunnelStep {
    pub option_id: String,
    pub label: String,
    pub position: i64,
    pub is_won: bool,
    pub is_lost: bool,
    /// Distinct records that ENTERED this stage in the window (from
    /// `stage_change` activities, plus records currently sitting in it that never
    /// generated one — i.e. created straight into it).
    pub entered: i64,
    /// Of those, how many went on to reach a LATER stage.
    pub advanced: i64,
    /// `advanced / entered`, 0.0–1.0.
    pub conversion_rate: f64,
    /// Mean time from entering this stage to leaving it, in whole hours. `None` when
    /// no record has left it yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_hours_in_stage: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunnelReport {
    pub object_id: String,
    pub field_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    pub steps: Vec<FunnelStep>,
}

/// The dock panel's header strip, in one request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrmSummary {
    pub objects: Vec<ObjectSummary>,
    pub total_records: i64,
    pub open_tasks: i64,
    pub overdue_tasks: i64,
    pub recent_activity: Vec<Activity>,
    /// The `deal` pipeline, when a deal object with a status field exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<PipelineReport>,
}

// ── Agent-facing tool DTOs ─────────────────────────────────────────────────────
//
// These are the shapes an LLM fills in, so they take SLUGS, not ids, and they are
// deliberately narrower than the full CRUD surface: an agent that can rewrite the
// schema is an agent that can destroy the CRM. Every tool below reads or writes
// records and activities only.

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolSearchRequest {
    pub query: String,
    /// Object slug. Absent = search everything.
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolFindRecordRequest {
    /// Object slug.
    pub object: String,
    /// Field slug to match on. Absent = the object's title field.
    #[serde(default)]
    pub field: Option<String>,
    pub value: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCreateRecordRequest {
    /// Object slug.
    pub object: String,
    /// Keyed by field SLUG. Select/status values may be given as labels.
    #[serde(default)]
    pub values: ValueBag,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolUpdateRecordRequest {
    pub record_id: String,
    #[serde(default)]
    pub values: ValueBag,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolLogActivityRequest {
    pub record_id: String,
    #[serde(default = "default_note_kind")]
    pub kind: ActivityKind,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub due_at: Option<String>,
}

fn default_note_kind() -> ActivityKind {
    ActivityKind::Note
}

/// The envelope every `/tools/*` route returns.
///
/// A uniform `{ ok, summary, data }` rather than the raw domain type: the caller is
/// a model, and a one-line natural-language `summary` is what actually lands in the
/// transcript. `data` is there for a tool body that wants to chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResponse {
    pub ok: bool,
    /// One line, human-readable. "Created deal “Acme renewal” (rec_01J…)."
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ToolResponse {
    pub fn ok(summary: impl Into<String>, data: Value) -> Self {
        Self {
            ok: true,
            summary: summary.into(),
            data: Some(data),
        }
    }

    pub fn failed(summary: impl Into<String>) -> Self {
        Self {
            ok: false,
            summary: summary.into(),
            data: None,
        }
    }
}

// ── Seeded schema ids ──────────────────────────────────────────────────────────
//
// DETERMINISTIC, not generated. Two reasons, and both are load-bearing:
//
//   1. Seeding is `INSERT OR IGNORE` on these exact ids, so re-running the seed on
//      an existing database is a no-op instead of a duplicate schema.
//   2. The dock panel, the agent tools and the manifest all reference the standard
//      schema. If the ids were random, every one of them would have to look them up
//      by slug first — and a panel that renders the deal board needs to know
//      `fld_deal_stage` at build time, not at runtime.
//
// Never renumber one of these. They are in shipped databases the moment the app is
// installed once.

pub const OBJ_COMPANY: &str = "obj_company";
pub const OBJ_PERSON: &str = "obj_person";
pub const OBJ_DEAL: &str = "obj_deal";
pub const OBJ_NOTE: &str = "obj_note";
pub const OBJ_TASK: &str = "obj_task";

/// The five standard object ids, in sidebar order.
pub const STANDARD_OBJECT_IDS: [&str; 5] = [OBJ_COMPANY, OBJ_PERSON, OBJ_DEAL, OBJ_NOTE, OBJ_TASK];

// company
pub const FLD_COMPANY_NAME: &str = "fld_company_name";
pub const FLD_COMPANY_DOMAIN: &str = "fld_company_domain";
pub const FLD_COMPANY_WEBSITE: &str = "fld_company_website";
pub const FLD_COMPANY_STATUS: &str = "fld_company_status";
pub const FLD_COMPANY_INDUSTRY: &str = "fld_company_industry";
pub const FLD_COMPANY_EMPLOYEES: &str = "fld_company_employees";
pub const FLD_COMPANY_ARR: &str = "fld_company_arr";
pub const FLD_COMPANY_PHONE: &str = "fld_company_phone";
pub const FLD_COMPANY_LOCATION: &str = "fld_company_location";
pub const FLD_COMPANY_DESCRIPTION: &str = "fld_company_description";
pub const FLD_COMPANY_OWNER: &str = "fld_company_owner";
pub const FLD_COMPANY_TAGS: &str = "fld_company_tags";

pub const OPT_COMPANY_STATUS_LEAD: &str = "opt_company_status_lead";
pub const OPT_COMPANY_STATUS_PROSPECT: &str = "opt_company_status_prospect";
pub const OPT_COMPANY_STATUS_CUSTOMER: &str = "opt_company_status_customer";
pub const OPT_COMPANY_STATUS_CHURNED: &str = "opt_company_status_churned";

// person
pub const FLD_PERSON_NAME: &str = "fld_person_name";
pub const FLD_PERSON_EMAIL: &str = "fld_person_email";
pub const FLD_PERSON_PHONE: &str = "fld_person_phone";
pub const FLD_PERSON_JOB_TITLE: &str = "fld_person_job_title";
pub const FLD_PERSON_COMPANY: &str = "fld_person_company";
pub const FLD_PERSON_LINKEDIN: &str = "fld_person_linkedin";
pub const FLD_PERSON_LOCATION: &str = "fld_person_location";
pub const FLD_PERSON_OWNER: &str = "fld_person_owner";
pub const FLD_PERSON_TAGS: &str = "fld_person_tags";
pub const FLD_PERSON_NOTES: &str = "fld_person_notes";

// deal
pub const FLD_DEAL_NAME: &str = "fld_deal_name";
pub const FLD_DEAL_STAGE: &str = "fld_deal_stage";
pub const FLD_DEAL_AMOUNT: &str = "fld_deal_amount";
pub const FLD_DEAL_PROBABILITY: &str = "fld_deal_probability";
pub const FLD_DEAL_CLOSE_DATE: &str = "fld_deal_close_date";
pub const FLD_DEAL_COMPANY: &str = "fld_deal_company";
pub const FLD_DEAL_CONTACT: &str = "fld_deal_contact";
pub const FLD_DEAL_OWNER: &str = "fld_deal_owner";
pub const FLD_DEAL_SOURCE: &str = "fld_deal_source";
pub const FLD_DEAL_DESCRIPTION: &str = "fld_deal_description";

pub const OPT_DEAL_STAGE_LEAD: &str = "opt_deal_stage_lead";
pub const OPT_DEAL_STAGE_QUALIFIED: &str = "opt_deal_stage_qualified";
pub const OPT_DEAL_STAGE_PROPOSAL: &str = "opt_deal_stage_proposal";
pub const OPT_DEAL_STAGE_NEGOTIATION: &str = "opt_deal_stage_negotiation";
pub const OPT_DEAL_STAGE_WON: &str = "opt_deal_stage_won";
pub const OPT_DEAL_STAGE_LOST: &str = "opt_deal_stage_lost";

pub const OPT_DEAL_SOURCE_INBOUND: &str = "opt_deal_source_inbound";
pub const OPT_DEAL_SOURCE_OUTBOUND: &str = "opt_deal_source_outbound";
pub const OPT_DEAL_SOURCE_REFERRAL: &str = "opt_deal_source_referral";
pub const OPT_DEAL_SOURCE_PARTNER: &str = "opt_deal_source_partner";
pub const OPT_DEAL_SOURCE_EVENT: &str = "opt_deal_source_event";
pub const OPT_DEAL_SOURCE_OTHER: &str = "opt_deal_source_other";

// note
pub const FLD_NOTE_SUBJECT: &str = "fld_note_subject";
pub const FLD_NOTE_BODY: &str = "fld_note_body";
pub const FLD_NOTE_AUTHOR: &str = "fld_note_author";
pub const FLD_NOTE_PINNED: &str = "fld_note_pinned";

// task
pub const FLD_TASK_NAME: &str = "fld_task_name";
pub const FLD_TASK_STATUS: &str = "fld_task_status";
pub const FLD_TASK_ASSIGNEE: &str = "fld_task_assignee";
pub const FLD_TASK_DUE_DATE: &str = "fld_task_due_date";
pub const FLD_TASK_PRIORITY: &str = "fld_task_priority";
pub const FLD_TASK_NOTES: &str = "fld_task_notes";

pub const OPT_TASK_STATUS_TODO: &str = "opt_task_status_todo";
pub const OPT_TASK_STATUS_IN_PROGRESS: &str = "opt_task_status_in_progress";
pub const OPT_TASK_STATUS_DONE: &str = "opt_task_status_done";
pub const OPT_TASK_STATUS_CANCELLED: &str = "opt_task_status_cancelled";

pub const OPT_TASK_PRIORITY_LOW: &str = "opt_task_priority_low";
pub const OPT_TASK_PRIORITY_MEDIUM: &str = "opt_task_priority_medium";
pub const OPT_TASK_PRIORITY_HIGH: &str = "opt_task_priority_high";
pub const OPT_TASK_PRIORITY_URGENT: &str = "opt_task_priority_urgent";

// views
pub const VIEW_COMPANY_ALL: &str = "view_company_all";
pub const VIEW_PERSON_ALL: &str = "view_person_all";
pub const VIEW_DEAL_ALL: &str = "view_deal_all";
pub const VIEW_DEAL_PIPELINE: &str = "view_deal_pipeline";
pub const VIEW_NOTE_ALL: &str = "view_note_all";
pub const VIEW_TASK_ALL: &str = "view_task_all";
pub const VIEW_TASK_BOARD: &str = "view_task_board";

// ── Test fixtures ──────────────────────────────────────────────────────────────

#[cfg(test)]
impl Object {
    /// A minimal object for unit tests that need one without a store.
    pub fn sample() -> Self {
        let now = now_rfc3339();
        Self {
            id: OBJ_DEAL.to_string(),
            slug: "deal".to_string(),
            singular: "Deal".to_string(),
            plural: "Deals".to_string(),
            icon: Some("target".to_string()),
            description: None,
            title_field_id: Some(FLD_DEAL_NAME.to_string()),
            is_standard: true,
            position: 2,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

#[cfg(test)]
impl Record {
    pub fn sample(object_id: &str) -> Self {
        let now = now_rfc3339();
        Self {
            id: new_id(ID_RECORD),
            object_id: object_id.to_string(),
            title: "Acme renewal".to_string(),
            values: ValueBag::new(),
            deleted_at: None,
            created_by: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_sort_in_creation_order() {
        let mut ids = Vec::new();
        for _ in 0..8 {
            ids.push(new_id(ID_RECORD));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "ULID-ish ids must sort in creation order");
    }

    #[test]
    fn ids_are_unique_within_a_millisecond() {
        let ids: std::collections::HashSet<String> =
            (0..1000).map(|_| new_id(ID_RECORD)).collect();
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn timestamps_are_fixed_width_and_lexicographically_ordered() {
        let a = now_rfc3339();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = now_rfc3339();
        assert_eq!(a.len(), b.len(), "precision must not vary: {a} vs {b}");
        assert!(a.ends_with('Z') && b.ends_with('Z'));
        assert!(a < b, "{a} must sort before {b}");
    }

    #[test]
    fn datetime_normalization_folds_offsets_to_utc() {
        assert_eq!(
            normalize_datetime("2026-08-10T16:03:11+02:00").unwrap(),
            "2026-08-10T14:03:11.000Z"
        );
        assert_eq!(
            normalize_datetime("2026-08-10").unwrap(),
            "2026-08-10T00:00:00.000Z"
        );
        assert_eq!(
            normalize_datetime("2026-08-10T14:03").unwrap(),
            "2026-08-10T14:03:00.000Z"
        );
        assert!(normalize_datetime("not a date").is_none());
    }

    #[test]
    fn dates_keep_no_time_component() {
        assert_eq!(normalize_date("2026-03-31").unwrap(), "2026-03-31");
        assert_eq!(
            normalize_date("2026-03-31T23:00:00Z").unwrap(),
            "2026-03-31"
        );
    }

    #[test]
    fn reserved_and_malformed_slugs_are_rejected() {
        assert!(is_valid_slug("stage"));
        assert!(is_valid_slug("close_date_2"));
        assert!(!is_valid_slug("created_at"), "reserved");
        assert!(!is_valid_slug("title"), "reserved");
        assert!(!is_valid_slug("Stage"), "must be lowercase");
        assert!(!is_valid_slug("2stage"), "must start with a letter");
        assert!(!is_valid_slug("sta ge"));
        assert!(!is_valid_slug("sta\"ge"), "no quote may reach a JSON path");
        assert!(!is_valid_slug("sta$ge"));
        assert!(!is_valid_slug(""));
        assert!(!is_valid_slug(&"a".repeat(MAX_SLUG_LEN + 1)));
    }

    #[test]
    fn slugify_produces_only_valid_slugs() {
        assert_eq!(slugify("First Name").unwrap(), "first_name");
        assert_eq!(slugify("  ARR ($) ").unwrap(), "arr");
        assert_eq!(slugify("2026 Target").unwrap(), "f_2026_target");
        assert_eq!(slugify("Created At").unwrap(), "created_at_field");
        assert!(slugify("!!!").is_none());
        for raw in ["First Name", "ARR ($)", "2026 Target", "Created At"] {
            let slug = slugify(raw).unwrap();
            assert!(is_valid_slug(&slug), "{raw} -> {slug}");
        }
    }

    #[test]
    fn field_types_round_trip_through_their_wire_string() {
        for t in FieldType::ALL {
            assert_eq!(FieldType::parse(t.as_str()), Some(t));
            let json = serde_json::to_string(&t).unwrap();
            assert_eq!(json, format!("\"{}\"", t.as_str()));
            assert_eq!(serde_json::from_str::<FieldType>(&json).unwrap(), t);
        }
        assert_eq!(FieldType::from_db("nonsense"), FieldType::Text);
    }

    #[test]
    fn activity_kinds_round_trip_through_their_wire_string() {
        for k in ActivityKind::ALL {
            assert_eq!(ActivityKind::parse(k.as_str()), Some(k));
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(json, format!("\"{}\"", k.as_str()));
        }
        assert_eq!(ActivityKind::from_db("nonsense"), ActivityKind::Note);
    }

    #[test]
    fn filter_trees_round_trip() {
        let filter = ViewFilter::And {
            filters: vec![
                ViewFilter::Condition(FilterCondition {
                    field_id: FLD_DEAL_STAGE.to_string(),
                    op: FilterOperator::IsAnyOf,
                    value: serde_json::json!([OPT_DEAL_STAGE_PROPOSAL, OPT_DEAL_STAGE_NEGOTIATION]),
                }),
                ViewFilter::Not {
                    filter: Box::new(ViewFilter::Condition(FilterCondition {
                        field_id: FLD_DEAL_AMOUNT.to_string(),
                        op: FilterOperator::IsEmpty,
                        value: Value::Null,
                    })),
                },
            ],
        };
        let json = serde_json::to_string(&filter).unwrap();
        assert!(json.contains("\"type\":\"and\""));
        assert!(json.contains("\"type\":\"condition\""));
        let back: ViewFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back, filter);
        assert!(!filter.is_empty());
        assert!(ViewFilter::all().is_empty());
    }

    #[test]
    fn merge_sources_round_trip_flattened() {
        let resolution = MergeFieldResolution {
            field_id: FLD_PERSON_PHONE.to_string(),
            source: MergeSource::Loser {
                record_id: "rec_x".to_string(),
            },
        };
        let json = serde_json::to_string(&resolution).unwrap();
        assert!(json.contains("\"source\":\"loser\""), "{json}");
        assert!(json.contains("\"record_id\":\"rec_x\""), "{json}");
        let back: MergeFieldResolution = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, resolution.source);
    }

    #[test]
    fn page_derives_has_more_rather_than_trusting_the_caller() {
        let page = Page::new(vec![1, 2, 3], 10, 3, 0);
        assert!(page.has_more);
        let page = Page::new(vec![1, 2, 3], 3, 3, 0);
        assert!(!page.has_more);
        let page = Page::new(vec![9, 10], 10, 3, 8);
        assert!(!page.has_more);
    }

    #[test]
    fn option_resolution_accepts_ids_and_labels() {
        let config = FieldConfig {
            options: vec![
                SelectOption::new(OPT_DEAL_STAGE_LEAD, "Lead", 0),
                SelectOption::new(OPT_DEAL_STAGE_WON, "Won", 4).won(),
            ],
            ..Default::default()
        };
        assert_eq!(
            config.resolve_option(OPT_DEAL_STAGE_LEAD).unwrap().label,
            "Lead"
        );
        assert_eq!(config.resolve_option("won").unwrap().id, OPT_DEAL_STAGE_WON);
        assert!(config.resolve_option("nope").is_none());
        assert!(config.option(OPT_DEAL_STAGE_WON).unwrap().is_terminal());
    }

    #[test]
    fn seeded_ids_are_unique() {
        let ids = [
            OBJ_COMPANY,
            OBJ_PERSON,
            OBJ_DEAL,
            OBJ_NOTE,
            OBJ_TASK,
            FLD_COMPANY_NAME,
            FLD_COMPANY_DOMAIN,
            FLD_COMPANY_WEBSITE,
            FLD_COMPANY_STATUS,
            FLD_COMPANY_INDUSTRY,
            FLD_COMPANY_EMPLOYEES,
            FLD_COMPANY_ARR,
            FLD_COMPANY_PHONE,
            FLD_COMPANY_LOCATION,
            FLD_COMPANY_DESCRIPTION,
            FLD_COMPANY_OWNER,
            FLD_COMPANY_TAGS,
            FLD_PERSON_NAME,
            FLD_PERSON_EMAIL,
            FLD_PERSON_PHONE,
            FLD_PERSON_JOB_TITLE,
            FLD_PERSON_COMPANY,
            FLD_PERSON_LINKEDIN,
            FLD_PERSON_LOCATION,
            FLD_PERSON_OWNER,
            FLD_PERSON_TAGS,
            FLD_PERSON_NOTES,
            FLD_DEAL_NAME,
            FLD_DEAL_STAGE,
            FLD_DEAL_AMOUNT,
            FLD_DEAL_PROBABILITY,
            FLD_DEAL_CLOSE_DATE,
            FLD_DEAL_COMPANY,
            FLD_DEAL_CONTACT,
            FLD_DEAL_OWNER,
            FLD_DEAL_SOURCE,
            FLD_DEAL_DESCRIPTION,
            FLD_NOTE_SUBJECT,
            FLD_NOTE_BODY,
            FLD_NOTE_AUTHOR,
            FLD_NOTE_PINNED,
            FLD_TASK_NAME,
            FLD_TASK_STATUS,
            FLD_TASK_ASSIGNEE,
            FLD_TASK_DUE_DATE,
            FLD_TASK_PRIORITY,
            FLD_TASK_NOTES,
            VIEW_COMPANY_ALL,
            VIEW_PERSON_ALL,
            VIEW_DEAL_ALL,
            VIEW_DEAL_PIPELINE,
            VIEW_NOTE_ALL,
            VIEW_TASK_ALL,
            VIEW_TASK_BOARD,
        ];
        let unique: std::collections::HashSet<&&str> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "a seeded id is duplicated");
    }
}
