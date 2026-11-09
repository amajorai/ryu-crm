use super::*;
use crate::store::parse_csv;

fn csv_body(object: &str, csv: &str) -> CreateImportRequest {
    CreateImportRequest {
        object_id: object.to_string(),
        filename: Some("people.csv".to_string()),
        csv: csv.to_string(),
        delimiter: None,
        has_header: None,
    }
}

async fn seed_person(state: &AppState, name: &str, email: &str) -> Record {
    let mut values = ValueBag::new();
    values.insert("name".to_string(), json!(name));
    values.insert("email".to_string(), json!(email));
    state
        .store
        .create_record(
            OBJ_PERSON,
            &CreateRecordRequest {
                values,
                created_by: None,
            },
        )
        .await
        .expect("store")
        .expect("valid")
}

/// Map every column by field slug, with no dedupe.
fn straight_mapping(count: usize, slugs: &[&str]) -> MappingBody {
    MappingBody {
        mappings: (0..count)
            .map(|index| MappingEntryBody {
                column_index: index,
                field_id: slugs.get(index).map(|s| (*s).to_string()),
                create_field: None,
            })
            .collect(),
        dedupe: ImportDedupe::default(),
    }
}

// ── Upload ──

#[tokio::test]
async fn upload_infers_columns_and_prefills_the_mapping() {
    let state = AppState::in_memory().expect("state");
    let job = create_import(
        State(state.clone()),
        Json(csv_body(
            "person",
            "Name,Email,Job Title\nAda Lovelace,ada@example.com,Analyst\nGrace Hopper,grace@example.com,Admiral\n",
        )),
    )
    .await
    .expect("upload")
    .0;

    assert_eq!(job.row_count, 2, "the header is not a data row");
    assert_eq!(job.columns.len(), 3);
    assert_eq!(job.status, ImportStatus::Draft);
    // The store's suggester matched the headers, and the mapping was pre-filled
    // from those suggestions.
    assert_eq!(
        job.columns[1].suggested_field_id.as_deref(),
        Some(FLD_PERSON_EMAIL)
    );
    assert_eq!(
        job.mappings[1].field_id.as_deref(),
        Some(FLD_PERSON_EMAIL),
        "a matching header should not need a human"
    );
}

#[tokio::test]
async fn upload_404s_on_an_unknown_object_instead_of_500ing() {
    let state = AppState::in_memory().expect("state");
    let error = create_import(
        State(state.clone()),
        Json(csv_body("aliens", "Name\nAda\n")),
    )
    .await
    .expect_err("unknown object");
    assert!(matches!(error, ApiError::NotFound(_)), "got {error:?}");
}

#[tokio::test]
async fn upload_rejects_a_multi_character_delimiter() {
    let state = AppState::in_memory().expect("state");
    let mut body = csv_body("person", "Name\nAda\n");
    body.delimiter = Some("||".to_string());
    let error = create_import(State(state.clone()), Json(body))
        .await
        .expect_err("bad delimiter");
    assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
}

// ── Mapping ──

#[tokio::test]
async fn mapping_rejects_a_column_past_the_end_of_the_file() {
    let state = AppState::in_memory().expect("state");
    let job = create_import(
        State(state.clone()),
        Json(csv_body("person", "Name\nAda Lovelace\n")),
    )
    .await
    .expect("upload")
    .0;

    let error = put_mapping(
        State(state.clone()),
        Path(job.id.clone()),
        Json(MappingBody {
            mappings: vec![MappingEntryBody {
                column_index: 7,
                field_id: Some("name".to_string()),
                create_field: None,
            }],
            dedupe: ImportDedupe::default(),
        }),
    )
    .await
    .expect_err("out of range");
    assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
}

#[tokio::test]
async fn mapping_rejects_an_unknown_field_rather_than_silently_ignoring_the_column() {
    let state = AppState::in_memory().expect("state");
    let job = create_import(
        State(state.clone()),
        Json(csv_body("person", "Name\nAda Lovelace\n")),
    )
    .await
    .expect("upload")
    .0;

    let error = put_mapping(
        State(state.clone()),
        Path(job.id.clone()),
        Json(straight_mapping(1, &["nonexistent_field"])),
    )
    .await
    .expect_err("unknown field");
    assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
}

#[tokio::test]
async fn mapping_rejects_two_columns_pointing_at_one_field() {
    let state = AppState::in_memory().expect("state");
    let job = create_import(
        State(state.clone()),
        Json(csv_body("person", "First,Second\nAda,Lovelace\n")),
    )
    .await
    .expect("upload")
    .0;

    let error = put_mapping(
        State(state.clone()),
        Path(job.id.clone()),
        Json(straight_mapping(2, &["name", "name"])),
    )
    .await
    .expect_err("duplicate target");
    assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
}

/// The silent-no-op trap: a dedupe key nothing maps to makes `find_import_match`
/// return "no match" for every row, so an import the user set up to MERGE
/// duplicates instead creates one duplicate per row.
#[tokio::test]
async fn mapping_rejects_a_dedupe_key_that_no_column_feeds() {
    let state = AppState::in_memory().expect("state");
    let job = create_import(
        State(state.clone()),
        Json(csv_body("person", "Name\nAda Lovelace\n")),
    )
    .await
    .expect("upload")
    .0;

    let mut body = straight_mapping(1, &["name"]);
    body.dedupe = ImportDedupe {
        match_field_ids: vec!["email".to_string()],
        strategy: DedupeStrategy::Update,
    };
    let error = put_mapping(State(state.clone()), Path(job.id.clone()), Json(body))
        .await
        .expect_err("unmapped dedupe key");
    assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
}

#[tokio::test]
async fn mapping_creates_a_field_for_a_column_with_no_home() {
    let state = AppState::in_memory().expect("state");
    let job = create_import(
        State(state.clone()),
        Json(csv_body(
            "person",
            "Name,Slack Handle\nAda Lovelace,@ada\nGrace Hopper,@grace\n",
        )),
    )
    .await
    .expect("upload")
    .0;

    let saved = put_mapping(
        State(state.clone()),
        Path(job.id.clone()),
        Json(MappingBody {
            mappings: vec![
                MappingEntryBody {
                    column_index: 0,
                    field_id: Some("name".to_string()),
                    create_field: None,
                },
                MappingEntryBody {
                    column_index: 1,
                    field_id: None,
                    // Nothing but the intent: name, slug and type all come from
                    // the column.
                    create_field: Some(NewFieldBody::default()),
                },
            ],
            dedupe: ImportDedupe::default(),
        }),
    )
    .await
    .expect("mapping")
    .0;

    let created = saved["created_fields"]
        .as_array()
        .expect("created_fields")
        .clone();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0]["slug"], json!("slack_handle"));
    assert_eq!(created[0]["name"], json!("Slack Handle"));
    assert_eq!(created[0]["field_type"], json!("text"));

    // The column is bound to the field that was just built, not left dangling.
    let field_id = created[0]["id"].as_str().expect("id").to_string();
    let bound = saved["mappings"]
        .as_array()
        .expect("mappings")
        .iter()
        .find(|m| m["column_index"] == json!(1))
        .expect("column 1");
    assert_eq!(bound["field_id"], json!(field_id));
}

#[tokio::test]
async fn mapping_refuses_a_column_that_asks_for_both_an_existing_and_a_new_field() {
    let state = AppState::in_memory().expect("state");
    let job = create_import(
        State(state.clone()),
        Json(csv_body("person", "Name\nAda Lovelace\n")),
    )
    .await
    .expect("upload")
    .0;

    let error = put_mapping(
        State(state.clone()),
        Path(job.id.clone()),
        Json(MappingBody {
            mappings: vec![MappingEntryBody {
                column_index: 0,
                field_id: Some("name".to_string()),
                create_field: Some(NewFieldBody::default()),
            }],
            dedupe: ImportDedupe::default(),
        }),
    )
    .await
    .expect_err("ambiguous");
    assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
}

mod apply;
mod export;
