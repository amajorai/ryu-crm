// ── Seeds ──────────────────────────────────────────────────────────────────────
//
// Every seeded row uses a DETERMINISTIC id from `models` and is written with
// `INSERT OR IGNORE`, so seeding is idempotent: it runs on every open and is a
// no-op on an existing database. That is also what makes it safe for the seed set
// to GROW in a later version — new rows appear, existing ones are never rewritten,
// and a user's edits to a standard field's name or options survive an upgrade.

/// One seed field, in the compact form the tables below are written in.
struct SeedField {
    id: &'static str,
    slug: &'static str,
    name: &'static str,
    field_type: FieldType,
    required: bool,
    unique: bool,
    system: bool,
    config: FieldConfig,
}

pub(super) fn sf(
    id: &'static str,
    slug: &'static str,
    name: &'static str,
    field_type: FieldType,
) -> SeedField {
    SeedField {
        id,
        slug,
        name,
        field_type,
        required: false,
        unique: false,
        system: false,
        config: FieldConfig::default(),
    }
}

impl SeedField {
    /// The object's display-name field: required and undeletable.
    fn title(mut self) -> Self {
        self.required = true;
        self.system = true;
        self
    }

    fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    fn required(mut self) -> Self {
        self.required = true;
        self
    }

    fn options(mut self, options: Vec<SelectOption>) -> Self {
        self.config.options = options;
        self
    }

    fn relation(mut self, target: &str, multiple: bool, inverse: &str) -> Self {
        self.config.relation_object_id = Some(target.to_string());
        self.config.relation_multiple = multiple;
        self.config.relation_inverse_label = Some(inverse.to_string());
        self
    }

    fn currency(mut self, code: &str) -> Self {
        self.config.currency_code = Some(code.to_string());
        self
    }
}

pub(super) fn seed_objects() -> Vec<(Object, Vec<SeedField>, Vec<View>)> {
    let now = now_rfc3339();
    let object = |id: &str,
                  slug: &str,
                  singular: &str,
                  plural: &str,
                  icon: &str,
                  title: &str,
                  position: i64| Object {
        id: id.to_string(),
        slug: slug.to_string(),
        singular: singular.to_string(),
        plural: plural.to_string(),
        icon: Some(icon.to_string()),
        description: None,
        title_field_id: Some(title.to_string()),
        is_standard: true,
        position,
        created_at: now.clone(),
        updated_at: now.clone(),
    };
    let view = |id: &str,
                object_id: &str,
                name: &str,
                kind: ViewKind,
                visible: &[&str],
                group_by: Option<&str>,
                is_default: bool,
                position: i64| View {
        id: id.to_string(),
        object_id: object_id.to_string(),
        name: name.to_string(),
        kind,
        filter: None,
        sorts: vec![ViewSort::desc("updated_at")],
        visible_field_ids: visible.iter().map(|s| (*s).to_string()).collect(),
        group_by_field_id: group_by.map(str::to_string),
        is_default,
        position,
        created_at: now.clone(),
        updated_at: now.clone(),
    };

    vec![
        (
            object(
                OBJ_COMPANY,
                "company",
                "Company",
                "Companies",
                "building-2",
                FLD_COMPANY_NAME,
                0,
            ),
            vec![
                sf(FLD_COMPANY_NAME, "name", "Name", FieldType::Text).title(),
                sf(FLD_COMPANY_DOMAIN, "domain", "Domain", FieldType::Text),
                sf(FLD_COMPANY_WEBSITE, "website", "Website", FieldType::Url),
                sf(FLD_COMPANY_STATUS, "status", "Status", FieldType::Status).options(vec![
                    SelectOption::new(OPT_COMPANY_STATUS_LEAD, "Lead", 0).with_color("slate"),
                    SelectOption::new(OPT_COMPANY_STATUS_PROSPECT, "Prospect", 1)
                        .with_color("blue"),
                    SelectOption::new(OPT_COMPANY_STATUS_CUSTOMER, "Customer", 2)
                        .with_color("green")
                        .won(),
                    SelectOption::new(OPT_COMPANY_STATUS_CHURNED, "Churned", 3)
                        .with_color("red")
                        .lost(),
                ]),
                sf(
                    FLD_COMPANY_INDUSTRY,
                    "industry",
                    "Industry",
                    FieldType::Select,
                )
                .options(vec![
                    SelectOption::new("opt_company_industry_saas", "SaaS", 0),
                    SelectOption::new("opt_company_industry_fintech", "Fintech", 1),
                    SelectOption::new("opt_company_industry_healthcare", "Healthcare", 2),
                    SelectOption::new("opt_company_industry_ecommerce", "E-commerce", 3),
                    SelectOption::new("opt_company_industry_manufacturing", "Manufacturing", 4),
                    SelectOption::new("opt_company_industry_media", "Media", 5),
                    SelectOption::new("opt_company_industry_education", "Education", 6),
                    SelectOption::new("opt_company_industry_nonprofit", "Nonprofit", 7),
                    SelectOption::new("opt_company_industry_other", "Other", 8),
                ]),
                sf(
                    FLD_COMPANY_EMPLOYEES,
                    "employees",
                    "Employees",
                    FieldType::Number,
                ),
                sf(FLD_COMPANY_ARR, "arr", "ARR", FieldType::Currency).currency("USD"),
                sf(FLD_COMPANY_PHONE, "phone", "Phone", FieldType::Phone),
                sf(
                    FLD_COMPANY_LOCATION,
                    "location",
                    "Location",
                    FieldType::Text,
                ),
                sf(
                    FLD_COMPANY_DESCRIPTION,
                    "description",
                    "Description",
                    FieldType::LongText,
                ),
                sf(FLD_COMPANY_OWNER, "owner", "Owner", FieldType::User),
                sf(FLD_COMPANY_TAGS, "tags", "Tags", FieldType::MultiSelect),
            ],
            vec![view(
                VIEW_COMPANY_ALL,
                OBJ_COMPANY,
                "All companies",
                ViewKind::Table,
                &[
                    FLD_COMPANY_NAME,
                    FLD_COMPANY_DOMAIN,
                    FLD_COMPANY_STATUS,
                    FLD_COMPANY_INDUSTRY,
                    FLD_COMPANY_ARR,
                    FLD_COMPANY_OWNER,
                ],
                None,
                true,
                0,
            )],
        ),
        (
            object(
                OBJ_PERSON,
                "person",
                "Person",
                "People",
                "user-round",
                FLD_PERSON_NAME,
                1,
            ),
            vec![
                sf(FLD_PERSON_NAME, "name", "Name", FieldType::Text).title(),
                // The canonical dedupe key, and the one seeded unique field.
                // Uniqueness ignores empty values, so a roster with blank emails
                // still imports.
                sf(FLD_PERSON_EMAIL, "email", "Email", FieldType::Email).unique(),
                sf(FLD_PERSON_PHONE, "phone", "Phone", FieldType::Phone),
                sf(
                    FLD_PERSON_JOB_TITLE,
                    "job_title",
                    "Job title",
                    FieldType::Text,
                ),
                sf(
                    FLD_PERSON_COMPANY,
                    "company",
                    "Company",
                    FieldType::Relation,
                )
                .relation(OBJ_COMPANY, false, "People"),
                sf(FLD_PERSON_LINKEDIN, "linkedin", "LinkedIn", FieldType::Url),
                sf(FLD_PERSON_LOCATION, "location", "Location", FieldType::Text),
                sf(FLD_PERSON_OWNER, "owner", "Owner", FieldType::User),
                sf(FLD_PERSON_TAGS, "tags", "Tags", FieldType::MultiSelect),
                sf(FLD_PERSON_NOTES, "notes", "Notes", FieldType::LongText),
            ],
            vec![view(
                VIEW_PERSON_ALL,
                OBJ_PERSON,
                "All people",
                ViewKind::Table,
                &[
                    FLD_PERSON_NAME,
                    FLD_PERSON_EMAIL,
                    FLD_PERSON_JOB_TITLE,
                    FLD_PERSON_COMPANY,
                    FLD_PERSON_OWNER,
                ],
                None,
                true,
                0,
            )],
        ),
        (
            object(
                OBJ_DEAL,
                "deal",
                "Deal",
                "Deals",
                "target",
                FLD_DEAL_NAME,
                2,
            ),
            vec![
                sf(FLD_DEAL_NAME, "name", "Name", FieldType::Text).title(),
                // THE pipeline field: `PipelineRequest` defaults to the object's
                // first status field by position, which is this one.
                sf(FLD_DEAL_STAGE, "stage", "Stage", FieldType::Status)
                    .required()
                    .options(vec![
                        SelectOption::new(OPT_DEAL_STAGE_LEAD, "Lead", 0).with_color("slate"),
                        SelectOption::new(OPT_DEAL_STAGE_QUALIFIED, "Qualified", 1)
                            .with_color("blue"),
                        SelectOption::new(OPT_DEAL_STAGE_PROPOSAL, "Proposal", 2)
                            .with_color("indigo"),
                        SelectOption::new(OPT_DEAL_STAGE_NEGOTIATION, "Negotiation", 3)
                            .with_color("amber"),
                        SelectOption::new(OPT_DEAL_STAGE_WON, "Won", 4)
                            .with_color("green")
                            .won(),
                        SelectOption::new(OPT_DEAL_STAGE_LOST, "Lost", 5)
                            .with_color("red")
                            .lost(),
                    ]),
                sf(FLD_DEAL_AMOUNT, "amount", "Amount", FieldType::Currency).currency("USD"),
                sf(
                    FLD_DEAL_PROBABILITY,
                    "probability",
                    "Probability",
                    FieldType::Percent,
                ),
                sf(
                    FLD_DEAL_CLOSE_DATE,
                    "close_date",
                    "Close date",
                    FieldType::Date,
                ),
                sf(FLD_DEAL_COMPANY, "company", "Company", FieldType::Relation).relation(
                    OBJ_COMPANY,
                    false,
                    "Deals",
                ),
                sf(FLD_DEAL_CONTACT, "contact", "Contacts", FieldType::Relation)
                    .relation(OBJ_PERSON, true, "Deals"),
                sf(FLD_DEAL_OWNER, "owner", "Owner", FieldType::User),
                sf(FLD_DEAL_SOURCE, "source", "Source", FieldType::Select).options(vec![
                    SelectOption::new(OPT_DEAL_SOURCE_INBOUND, "Inbound", 0),
                    SelectOption::new(OPT_DEAL_SOURCE_OUTBOUND, "Outbound", 1),
                    SelectOption::new(OPT_DEAL_SOURCE_REFERRAL, "Referral", 2),
                    SelectOption::new(OPT_DEAL_SOURCE_PARTNER, "Partner", 3),
                    SelectOption::new(OPT_DEAL_SOURCE_EVENT, "Event", 4),
                    SelectOption::new(OPT_DEAL_SOURCE_OTHER, "Other", 5),
                ]),
                sf(
                    FLD_DEAL_DESCRIPTION,
                    "description",
                    "Description",
                    FieldType::LongText,
                ),
            ],
            vec![
                view(
                    VIEW_DEAL_ALL,
                    OBJ_DEAL,
                    "All deals",
                    ViewKind::Table,
                    &[
                        FLD_DEAL_NAME,
                        FLD_DEAL_STAGE,
                        FLD_DEAL_AMOUNT,
                        FLD_DEAL_CLOSE_DATE,
                        FLD_DEAL_COMPANY,
                        FLD_DEAL_OWNER,
                    ],
                    None,
                    true,
                    0,
                ),
                view(
                    VIEW_DEAL_PIPELINE,
                    OBJ_DEAL,
                    "Pipeline",
                    ViewKind::Board,
                    &[
                        FLD_DEAL_NAME,
                        FLD_DEAL_AMOUNT,
                        FLD_DEAL_CLOSE_DATE,
                        FLD_DEAL_COMPANY,
                    ],
                    Some(FLD_DEAL_STAGE),
                    false,
                    1,
                ),
            ],
        ),
        (
            object(
                OBJ_NOTE,
                "note",
                "Note",
                "Notes",
                "sticky-note",
                FLD_NOTE_SUBJECT,
                3,
            ),
            vec![
                // `subject`, not `title`: `title` is a RESERVED_SLUG (it is a real
                // column on `records`).
                sf(FLD_NOTE_SUBJECT, "subject", "Subject", FieldType::Text).title(),
                sf(FLD_NOTE_BODY, "body", "Body", FieldType::LongText),
                sf(FLD_NOTE_AUTHOR, "author", "Author", FieldType::User),
                sf(FLD_NOTE_PINNED, "pinned", "Pinned", FieldType::Checkbox),
            ],
            vec![view(
                VIEW_NOTE_ALL,
                OBJ_NOTE,
                "All notes",
                ViewKind::Table,
                &[FLD_NOTE_SUBJECT, FLD_NOTE_AUTHOR, FLD_NOTE_PINNED],
                None,
                true,
                0,
            )],
        ),
        (
            object(
                OBJ_TASK,
                "task",
                "Task",
                "Tasks",
                "circle-check",
                FLD_TASK_NAME,
                4,
            ),
            vec![
                sf(FLD_TASK_NAME, "name", "Name", FieldType::Text).title(),
                sf(FLD_TASK_STATUS, "status", "Status", FieldType::Status).options(vec![
                    SelectOption::new(OPT_TASK_STATUS_TODO, "To do", 0).with_color("slate"),
                    SelectOption::new(OPT_TASK_STATUS_IN_PROGRESS, "In progress", 1)
                        .with_color("blue"),
                    SelectOption::new(OPT_TASK_STATUS_DONE, "Done", 2)
                        .with_color("green")
                        .won(),
                    SelectOption::new(OPT_TASK_STATUS_CANCELLED, "Cancelled", 3)
                        .with_color("red")
                        .lost(),
                ]),
                sf(FLD_TASK_ASSIGNEE, "assignee", "Assignee", FieldType::User),
                sf(FLD_TASK_DUE_DATE, "due_date", "Due", FieldType::Datetime),
                sf(FLD_TASK_PRIORITY, "priority", "Priority", FieldType::Select).options(vec![
                    SelectOption::new(OPT_TASK_PRIORITY_LOW, "Low", 0),
                    SelectOption::new(OPT_TASK_PRIORITY_MEDIUM, "Medium", 1),
                    SelectOption::new(OPT_TASK_PRIORITY_HIGH, "High", 2),
                    SelectOption::new(OPT_TASK_PRIORITY_URGENT, "Urgent", 3),
                ]),
                sf(FLD_TASK_NOTES, "notes", "Notes", FieldType::LongText),
            ],
            vec![
                view(
                    VIEW_TASK_ALL,
                    OBJ_TASK,
                    "All tasks",
                    ViewKind::Table,
                    &[
                        FLD_TASK_NAME,
                        FLD_TASK_STATUS,
                        FLD_TASK_ASSIGNEE,
                        FLD_TASK_DUE_DATE,
                        FLD_TASK_PRIORITY,
                    ],
                    None,
                    true,
                    0,
                ),
                view(
                    VIEW_TASK_BOARD,
                    OBJ_TASK,
                    "Task board",
                    ViewKind::Board,
                    &[
                        FLD_TASK_NAME,
                        FLD_TASK_ASSIGNEE,
                        FLD_TASK_DUE_DATE,
                        FLD_TASK_PRIORITY,
                    ],
                    Some(FLD_TASK_STATUS),
                    false,
                    1,
                ),
            ],
        ),
    ]
}

pub(super) fn seed_standard_schema(conn: &Connection) -> Result<()> {
    let now = now_rfc3339();
    for (object, fields, views) in seed_objects() {
        conn.execute(
            "INSERT OR IGNORE INTO objects
               (id, slug, singular, plural, icon, description, title_field_id, is_standard, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9, ?10)",
            params![
                object.id,
                object.slug,
                object.singular,
                object.plural,
                object.icon,
                object.description,
                object.title_field_id,
                object.position,
                now,
                now
            ],
        )?;
        for (position, field) in fields.iter().enumerate() {
            conn.execute(
                "INSERT OR IGNORE INTO fields
                   (id, object_id, list_id, slug, name, field_type, config, description,
                    is_required, is_unique, is_system, position, created_at, updated_at)
                 VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    field.id,
                    object.id,
                    field.slug,
                    field.name,
                    field.field_type.as_str(),
                    field.config.encode(),
                    i64::from(field.required),
                    i64::from(field.unique),
                    i64::from(field.system),
                    position as i64,
                    now,
                    now
                ],
            )?;
        }
        for view in &views {
            conn.execute(
                "INSERT OR IGNORE INTO views
                   (id, object_id, name, kind, filter, sorts, visible_fields,
                    group_by_field_id, is_default, position, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    view.id,
                    view.object_id,
                    view.name,
                    view.kind.as_str(),
                    serde_json::to_string(&view.sorts)?,
                    serde_json::to_string(&view.visible_field_ids)?,
                    view.group_by_field_id,
                    i64::from(view.is_default),
                    view.position,
                    now,
                    now
                ],
            )?;
        }
    }
    Ok(())
}
