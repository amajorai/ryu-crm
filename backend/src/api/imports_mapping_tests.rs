//! The mapping screen's read: what `PUT .../mapping` saved is what the next
//! `GET .../mapping` hands back, with the columns and the field list beside it.

use super::*;

const CSV: &str = "Name,Email,Job Title\nAda Lovelace,ada@example.com,Analyst\nGrace Hopper,grace@example.com,Admiral\n";

fn upload() -> CreateImportRequest {
    CreateImportRequest {
        object_id: "person".to_string(),
        filename: Some("people.csv".to_string()),
        csv: CSV.to_string(),
        delimiter: None,
        has_header: None,
    }
}

fn entry(column_index: usize, field_id: Option<&str>) -> MappingEntryBody {
    MappingEntryBody {
        column_index,
        field_id: field_id.map(str::to_string),
        create_field: None,
    }
}

#[tokio::test]
async fn a_saved_mapping_and_dedupe_rule_read_back_from_the_mapping_screen() {
    let state = AppState::in_memory().expect("state");
    let job = create_import(State(state.clone()), Json(upload()))
        .await
        .expect("upload")
        .0;
    // The upload pre-filled the job title column; the edit below unmaps it.
    assert!(job.mappings[2].field_id.is_some());

    let saved = put_mapping(
        State(state.clone()),
        Path(job.id.clone()),
        Json(MappingBody {
            mappings: vec![
                entry(0, Some(FLD_PERSON_NAME)),
                entry(1, Some(FLD_PERSON_EMAIL)),
                entry(2, None),
            ],
            dedupe: ImportDedupe {
                match_field_ids: vec![FLD_PERSON_EMAIL.to_string()],
                strategy: DedupeStrategy::Update,
            },
        }),
    )
    .await
    .expect("a valid mapping is saved")
    .0;

    let screen = get_mapping(State(state.clone()), Path(job.id.clone()))
        .await
        .expect("mapping screen")
        .0;
    assert_eq!(screen["import_id"], json!(job.id));
    assert_eq!(screen["object_id"], json!(OBJ_PERSON));
    assert_eq!(screen["row_count"], json!(2));
    let mapped: Vec<(u64, Value)> = screen["mappings"]
        .as_array()
        .expect("mappings")
        .iter()
        .map(|m| (m["column_index"].as_u64().unwrap(), m["field_id"].clone()))
        .collect();
    assert_eq!(
        mapped,
        vec![
            (0, json!(FLD_PERSON_NAME)),
            (1, json!(FLD_PERSON_EMAIL)),
            (2, Value::Null),
        ]
    );
    assert_eq!(screen["mappings"], saved["mappings"]);
    assert_eq!(
        screen["dedupe"],
        json!({"match_field_ids": [FLD_PERSON_EMAIL], "strategy": "update"})
    );
    let columns = screen["columns"].as_array().expect("columns");
    assert_eq!(columns.len(), 3);
    assert_eq!(columns[1]["name"], json!("Email"));
    assert_eq!(columns[1]["samples"][0], json!("ada@example.com"));
    assert!(screen["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .any(|f| f["id"] == json!(FLD_PERSON_JOB_TITLE)));
}

#[tokio::test]
async fn the_mapping_screen_for_an_unknown_import_is_a_404() {
    let state = AppState::in_memory().expect("state");
    let error = get_mapping(State(state), Path("imp_missing".to_string()))
        .await
        .expect_err("no such import");
    assert!(matches!(error, ApiError::NotFound(_)), "got {error:?}");
}
