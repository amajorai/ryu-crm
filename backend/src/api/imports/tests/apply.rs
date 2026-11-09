use super::*;

// ── Preview, apply, idempotency ──

#[tokio::test]
async fn preview_and_apply_agree_and_dedupe_on_email() {
    let state = AppState::in_memory().expect("state");
    let existing = seed_person(&state, "Ada Lovelace", "ada@example.com").await;

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

    let mut body = straight_mapping(3, &["name", "email", "job_title"]);
    body.dedupe = ImportDedupe {
        match_field_ids: vec!["email".to_string()],
        strategy: DedupeStrategy::Update,
    };
    put_mapping(State(state.clone()), Path(job.id.clone()), Json(body))
        .await
        .expect("mapping");

    let preview = preview_import(State(state.clone()), Path(job.id.clone()))
        .await
        .expect("preview")
        .0;
    assert_eq!(preview.total_rows, 2);
    assert_eq!(preview.create_count, 1, "Grace is new");
    assert_eq!(preview.update_count, 1, "Ada matches on email");
    assert_eq!(preview.error_count, 0);
    assert!(
        preview.unmapped_columns.is_empty(),
        "every column was mapped"
    );

    let applied = apply_import(State(state.clone()), Path(job.id.clone()))
        .await
        .expect("apply")
        .0;
    // The apply must land exactly what the preview promised — same planner, so a
    // divergence here means someone gave one of them its own copy.
    assert_eq!(applied["result"]["created"], json!(1));
    assert_eq!(applied["result"]["updated"], json!(1));
    assert_eq!(applied["result"]["failed"], json!(0));
    assert_eq!(applied["import"]["status"], json!("applied"));
    assert_eq!(
        applied["result"]["updated_record_ids"],
        json!([existing.id]),
        "the update reused the seeded record rather than creating a twin"
    );

    // …and the update actually wrote through.
    let ada = state
        .store
        .get_record(&existing.id)
        .await
        .expect("store")
        .expect("record");
    assert_eq!(ada.values.get("job_title"), Some(&json!("Analyst")));
}

#[tokio::test]
async fn a_second_apply_is_refused_rather_than_double_importing() {
    let state = AppState::in_memory().expect("state");
    let job = create_import(
        State(state.clone()),
        Json(csv_body("person", "Name\nAda Lovelace\n")),
    )
    .await
    .expect("upload")
    .0;
    put_mapping(
        State(state.clone()),
        Path(job.id.clone()),
        Json(straight_mapping(1, &["name"])),
    )
    .await
    .expect("mapping");
    apply_import(State(state.clone()), Path(job.id.clone()))
        .await
        .expect("first apply");

    let error = apply_import(State(state.clone()), Path(job.id.clone()))
        .await
        .expect_err("second apply");
    assert!(matches!(error, ApiError::Conflict(_)), "got {error:?}");

    // Remapping an applied job is refused for the same reason, and with the same
    // status — `set_import_mapping` would otherwise report it as a 404.
    let error = put_mapping(
        State(state.clone()),
        Path(job.id.clone()),
        Json(straight_mapping(1, &["name"])),
    )
    .await
    .expect_err("remap after apply");
    assert!(matches!(error, ApiError::Conflict(_)), "got {error:?}");
}

#[tokio::test]
async fn an_unmapped_import_is_refused_before_it_fails_every_row() {
    let state = AppState::in_memory().expect("state");
    let job = create_import(
        State(state.clone()),
        // A header the suggester cannot place, so nothing is pre-mapped.
        Json(csv_body("person", "Zxq\nsomething\n")),
    )
    .await
    .expect("upload")
    .0;
    assert!(job.mappings.iter().all(|m| m.field_id.is_none()));

    let error = preview_import(State(state.clone()), Path(job.id.clone()))
        .await
        .expect_err("nothing mapped");
    assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
    let error = apply_import(State(state.clone()), Path(job.id))
        .await
        .expect_err("nothing mapped");
    assert!(matches!(error, ApiError::BadRequest(_)), "got {error:?}");
}

/// Currency and dates are the two places the import's coercion is invisible from
/// the wire types: a `"$1,234.56"` cell must land as 123456 CENTS, and a
/// `2026-03-31` close date must stay a bare calendar day.
#[tokio::test]
async fn import_coerces_money_to_cents_and_dates_to_calendar_days() {
    let state = AppState::in_memory().expect("state");
    let job = create_import(
        State(state.clone()),
        Json(csv_body(
            "deal",
            "Name,Stage,Amount,Close Date\nBig One,Proposal,\"$1,234.56\",2026-03-31\n",
        )),
    )
    .await
    .expect("upload")
    .0;
    put_mapping(
        State(state.clone()),
        Path(job.id.clone()),
        Json(straight_mapping(
            4,
            &["name", "stage", "amount", "close_date"],
        )),
    )
    .await
    .expect("mapping");
    let applied = apply_import(State(state.clone()), Path(job.id))
        .await
        .expect("apply")
        .0;

    let record_id = applied["result"]["created_record_ids"][0]
        .as_str()
        .expect("created id")
        .to_string();
    let record = state
        .store
        .get_record(&record_id)
        .await
        .expect("store")
        .expect("record");
    assert_eq!(record.values.get("amount"), Some(&json!(123_456)));
    assert_eq!(record.values.get("close_date"), Some(&json!("2026-03-31")));
    // "Proposal" is a LABEL; it must have resolved to the option id.
    assert_eq!(
        record.values.get("stage"),
        Some(&json!(OPT_DEAL_STAGE_PROPOSAL))
    );
}

// ── Type inference ──

#[test]
fn column_types_are_inferred_only_when_every_sample_agrees() {
    assert_eq!(
        infer_field_type(&["ada@example.com".into(), "grace@example.com".into()]),
        FieldType::Email
    );
    assert_eq!(
        infer_field_type(&["https://acme.com".into()]),
        FieldType::Url
    );
    assert_eq!(infer_field_type(&["$1,234.56".into()]), FieldType::Currency);
    assert_eq!(
        infer_field_type(&["42".into(), "7.5".into()]),
        FieldType::Number
    );
    assert_eq!(infer_field_type(&["2026-03-31".into()]), FieldType::Date);
    assert_eq!(
        infer_field_type(&["2026-03-31T09:00:00Z".into()]),
        FieldType::Datetime
    );
    assert_eq!(
        infer_field_type(&["yes".into(), "no".into()]),
        FieldType::Checkbox
    );
    assert_eq!(infer_field_type(&[]), FieldType::Text);
    assert_eq!(
        infer_field_type(&["x".repeat(LONG_TEXT_SAMPLE_LEN + 1)]),
        FieldType::LongText
    );
    // One dissenting sample drops the whole column back to text — inferring
    // `email` here would reject every other row of the import.
    assert_eq!(
        infer_field_type(&["ada@example.com".into(), "Ada Lovelace".into()]),
        FieldType::Text
    );
    // Never guessed: an option-backed field with no options rejects everything.
    for samples in [vec!["Lead".to_string()], vec!["A".into(), "B".into()]] {
        let inferred = infer_field_type(&samples);
        assert!(
            !inferred.is_option_backed(),
            "{samples:?} inferred {inferred:?}"
        );
    }
}
